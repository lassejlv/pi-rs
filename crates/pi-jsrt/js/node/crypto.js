// node:crypto — hashes, HMAC, ciphers, KDFs, key objects, signatures, key agreement, RSA
// encryption and WebCrypto (`crypto.subtle`), over ops/crypto.rs, ops/cipher.rs and ops/keys.rs.

const Buffer = internal.Buffer;
const { Transform, Writable } = internal.requireBuiltin("stream");
const DOMException = internal.DOMException;
const hashes = ["RSA-MD5", "RSA-RIPEMD160", "RSA-SHA1", "RSA-SHA1-2", "RSA-SHA224", "RSA-SHA256", "RSA-SHA3-224", "RSA-SHA3-256", "RSA-SHA3-384", "RSA-SHA3-512", "RSA-SHA384", "RSA-SHA512", "RSA-SHA512/224", "RSA-SHA512/256", "RSA-SM3", "blake2b512", "blake2s256", "id-rsassa-pkcs1-v1_5-with-sha3-224", "id-rsassa-pkcs1-v1_5-with-sha3-256", "id-rsassa-pkcs1-v1_5-with-sha3-384", "id-rsassa-pkcs1-v1_5-with-sha3-512", "md5", "md5-sha1", "md5WithRSAEncryption", "ripemd", "ripemd160", "ripemd160WithRSA", "rmd160", "sha1", "sha1WithRSAEncryption", "sha224", "sha224WithRSAEncryption", "sha256", "sha256WithRSAEncryption", "sha3-224", "sha3-256", "sha3-384", "sha3-512", "sha384", "sha384WithRSAEncryption", "sha512", "sha512-224", "sha512-224WithRSAEncryption", "sha512-256", "sha512-256WithRSAEncryption", "sha512WithRSAEncryption", "shake128", "shake256", "sm3", "sm3WithRSAEncryption", "ssl3-md5", "ssl3-sha1"];
const curves = ["prime256v1", "secp256k1", "secp384r1", "secp521r1"];

const codeError = (Ctor, message, code) => Object.assign(new Ctor(message), code ? { code } : {});

function describeReceived(value) {
  if (value === null) return "null";
  if (value === undefined) return "undefined";
  if (typeof value === "function") return `function ${value.name}`;
  if (typeof value === "object") return value.constructor?.name ? `an instance of ${value.constructor.name}` : "an object";
  let shown = String(value).slice(0, 25);
  if (typeof value === "string") shown = `'${shown}'`;
  return `type ${typeof value} (${shown})`;
}

function invalidArgType(name, expected, value) {
  const kind = name.includes(".") ? "property" : "argument";
  return codeError(TypeError, `The "${name}" ${kind} must be ${expected}. Received ${describeReceived(value)}`, "ERR_INVALID_ARG_TYPE");
}

function toBytes(data, encoding, name = "data") {
  if (typeof data === "string") return Buffer.from(data, encoding && encoding !== "buffer" ? encoding : "utf8");
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  if (data instanceof ArrayBuffer || (typeof SharedArrayBuffer !== "undefined" && data instanceof SharedArrayBuffer)) return new Uint8Array(data);
  throw invalidArgType(name, "of type string or an instance of Buffer, TypedArray, or DataView", data);
}

const asBuffer = (u8) => Buffer.from(u8.buffer, u8.byteOffset, u8.byteLength);
const encodeOut = (u8, encoding) => {
  const b = asBuffer(u8);
  return encoding && encoding !== "buffer" ? b.toString(encoding) : b;
};

function checkDigest(alg, style = "hash") {
  const name = typeof alg === "string" ? ops.digestName(alg) : undefined;
  if (!name) {
    if (style === "hash") throw new Error("Digest method not supported");
    if (style === "sign") throw codeError(TypeError, "Invalid digest", "ERR_CRYPTO_INVALID_DIGEST");
    throw codeError(TypeError, `Invalid digest: ${alg}`, "ERR_CRYPTO_INVALID_DIGEST");
  }
  return name;
}

// ---------------------------------------------------------------------------
// Hash / Hmac
// ---------------------------------------------------------------------------

const kHashHandle = Symbol("kHashHandle");

class Hash extends Transform {
  #alg;
  #handle;
  #done = false;
  constructor(alg, options) {
    super(options);
    if (alg instanceof Hash && options?.[kHashHandle]) {
      this.#alg = alg.#alg;
      this.#handle = options[kHashHandle];
      return;
    }
    if (typeof alg !== "string") throw invalidArgType("algorithm", "of type string", alg);
    const outputLength = options?.outputLength;
    if (outputLength !== undefined) validateUint32(outputLength, "options.outputLength");
    this.#alg = alg;
    this.#handle = ops.hashNew(alg, outputLength);
  }
  update(data, encoding) {
    if (this.#done) throw codeError(Error, "Digest already called", "ERR_CRYPTO_HASH_FINALIZED");
    if (typeof data !== "string" && !ArrayBuffer.isView(data)) throw invalidArgType("data", "of type string or an instance of Buffer, TypedArray, or DataView", data);
    ops.hashUpdate(this.#handle, toBytes(data, encoding));
    return this;
  }
  digest(encoding) {
    if (this.#done) throw codeError(Error, "Digest already called", "ERR_CRYPTO_HASH_FINALIZED");
    this.#done = true;
    return encodeOut(ops.hashDigest(this.#handle), encoding);
  }
  copy(options) {
    if (this.#done) throw codeError(Error, "Digest already called", "ERR_CRYPTO_HASH_FINALIZED");
    const outputLength = options?.outputLength;
    if (outputLength !== undefined) validateUint32(outputLength, "options.outputLength");
    return new Hash(this, { ...options, [kHashHandle]: ops.hashCopy(this.#handle, outputLength) });
  }
  _transform(chunk, encoding, cb) {
    this.update(chunk, encoding);
    cb();
  }
  _flush(cb) {
    this.push(this.digest());
    cb();
  }
}

function validateUint32(value, name) {
  if (typeof value !== "number") throw invalidArgType(name, "of type number", value);
  if (!Number.isInteger(value)) throw codeError(RangeError, `The value of "${name}" is out of range. It must be an integer. Received ${value}`, "ERR_OUT_OF_RANGE");
  if (value < 0 || value > 4294967295) throw codeError(RangeError, `The value of "${name}" is out of range. It must be >= 0 && <= 4294967295. Received ${value}`, "ERR_OUT_OF_RANGE");
}

class Hmac extends Transform {
  #handle;
  #done = false;
  constructor(alg, key, options) {
    super(options);
    if (typeof alg !== "string") throw invalidArgType("hmac", "of type string", alg);
    let keyBytes;
    if (key instanceof KeyObject) {
      if (key.type !== "secret") throw codeError(TypeError, `Invalid key object type ${key.type}, expected secret.`, "ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE");
      keyBytes = key[kSecret];
    } else if (isCryptoKey(key)) {
      keyBytes = key[kKeyObject][kSecret];
    } else {
      keyBytes = toBytes(key, options?.encoding, "key");
    }
    if (!ops.digestName(alg)) throw codeError(TypeError, `Invalid digest: ${alg}`, "ERR_CRYPTO_INVALID_DIGEST");
    this.#handle = ops.hmacNew(alg, keyBytes);
  }
  update(data, encoding) {
    if (this.#done) throw codeError(Error, "Digest already called", "ERR_CRYPTO_HASH_FINALIZED");
    ops.hashUpdate(this.#handle, toBytes(data, encoding));
    return this;
  }
  digest(encoding) {
    if (this.#done) return encodeOut(new Uint8Array(0), encoding);
    this.#done = true;
    return encodeOut(ops.hashDigest(this.#handle), encoding);
  }
  _transform(chunk, encoding, cb) {
    this.update(chunk, encoding);
    cb();
  }
  _flush(cb) {
    this.push(this.digest());
    cb();
  }
}

// ---------------------------------------------------------------------------
// Ciphers
// ---------------------------------------------------------------------------

function keyMaterial(key, encoding, name = "key") {
  if (key instanceof KeyObject) {
    if (key.type !== "secret") throw codeError(TypeError, `Invalid key object type ${key.type}, expected secret.`, "ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE");
    return key[kSecret];
  }
  if (isCryptoKey(key)) return key[kKeyObject][kSecret];
  return toBytes(key, encoding, name);
}

class CipherBase extends Transform {
  #handle;
  #decoder;
  #mode;
  constructor(cipher, key, iv, options, encrypt) {
    super(options);
    if (typeof cipher !== "string") throw invalidArgType("cipher", "of type string", cipher);
    const k = keyMaterial(key);
    const v = iv === null || iv === undefined ? null : toBytes(iv, undefined, "iv");
    this.#handle = ops.cipherNew(cipher, k, v, encrypt, options?.authTagLength);
    this.#mode = ops.cipherInfo(cipher)?.mode;
  }
  update(data, inputEncoding, outputEncoding) {
    const out = asBuffer(ops.cipherUpdate(this.#handle, toBytes(data, inputEncoding)));
    return this._out(out, outputEncoding);
  }
  final(outputEncoding) {
    const out = asBuffer(ops.cipherFinal(this.#handle));
    const r = this._out(out, outputEncoding, true);
    return r;
  }
  _out(buf, encoding, end) {
    if (!encoding || encoding === "buffer") return buf;
    if (encoding === "utf8" || encoding === "utf-8") {
      this.#decoder ??= new (internal.requireBuiltin("string_decoder").StringDecoder)("utf8");
      return end ? this.#decoder.end(buf) : this.#decoder.write(buf);
    }
    return buf.toString(encoding);
  }
  setAutoPadding(on = true) {
    ops.cipherCtl(this.#handle, "setAutoPadding", on);
    return this;
  }
  setAAD(aad, options) {
    const plaintextLength = options?.plaintextLength;
    if (this.#mode === "ccm" && plaintextLength === undefined) {
      throw codeError(TypeError, "options.plaintextLength required for CCM mode with AAD", "ERR_MISSING_ARGS");
    }
    ops.cipherCtl(this.#handle, "setAAD", toBytes(aad, undefined, "buffer"), plaintextLength);
    return this;
  }
  getAuthTag() {
    return asBuffer(ops.cipherCtl(this.#handle, "getAuthTag"));
  }
  setAuthTag(tag, encoding) {
    ops.cipherCtl(this.#handle, "setAuthTag", toBytes(tag, encoding, "buffer"));
    return this;
  }
  _transform(chunk, encoding, cb) {
    try {
      this.push(this.update(chunk, encoding));
      cb();
    } catch (err) {
      cb(err);
    }
  }
  _flush(cb) {
    try {
      this.push(this.final());
      cb();
    } catch (err) {
      cb(err);
    }
  }
}

class Cipheriv extends CipherBase {
  constructor(cipher, key, iv, options) {
    super(cipher, key, iv, options, true);
  }
}

class Decipheriv extends CipherBase {
  constructor(cipher, key, iv, options) {
    super(cipher, key, iv, options, false);
  }
}

const createCipheriv = (alg, key, iv, options) => new Cipheriv(alg, key, iv, options);
const createDecipheriv = (alg, key, iv, options) => new Decipheriv(alg, key, iv, options);

function getCipherInfo(name, options) {
  const info = ops.cipherInfo(String(name));
  if (!info) return undefined;
  if (options?.keyLength !== undefined && options.keyLength !== info.keyLength) return undefined;
  let ivLength = info.ivLength;
  if (options?.ivLength !== undefined && options.ivLength !== info.ivLength) {
    const n = options.ivLength;
    const ok = info.mode === "gcm" ? n >= 1 : info.mode === "ccm" ? n >= 7 && n <= 13 : info.mode === "ocb" ? n >= 1 && n <= 15 : false;
    if (!ok) return undefined;
    ivLength = n;
  }
  const out = { mode: info.mode, name: info.name, nid: info.nid };
  if (info.mode !== "stream") out.blockSize = info.blockSize;
  if (ivLength !== 0) out.ivLength = ivLength;
  out.keyLength = info.keyLength;
  return out;
}

// ---------------------------------------------------------------------------
// Key objects
// ---------------------------------------------------------------------------

const kHandle = Symbol("kHandle");
const kSecret = Symbol("kSecret");
const kKeyObject = Symbol("kKeyObject");

class KeyObject {
  constructor(type, handle) {
    if (type !== "secret" && type !== "public" && type !== "private") {
      throw codeError(TypeError, `The argument 'type' is invalid. Received '${type}'`, "ERR_INVALID_ARG_VALUE");
    }
    Object.defineProperty(this, "_type", { value: type });
    if (type === "secret") Object.defineProperty(this, kSecret, { value: handle });
    else Object.defineProperty(this, kHandle, { value: handle });
  }
  get type() {
    return this._type;
  }
  get asymmetricKeyType() {
    return this._type === "secret" ? undefined : this[kHandle].asym;
  }
  get asymmetricKeyDetails() {
    if (this._type === "secret") return undefined;
    const d = this[kHandle].details ?? {};
    const out = {};
    if (d.modulusLength !== undefined) {
      out.modulusLength = d.modulusLength;
      if (d.publicExponentHex !== undefined) out.publicExponent = BigInt(`0x${d.publicExponentHex}`);
      if (d.divisorLength !== undefined) out.divisorLength = d.divisorLength;
    }
    if (d.namedCurve !== undefined) out.namedCurve = d.namedCurve;
    return out;
  }
  get symmetricKeySize() {
    return this._type === "secret" ? this[kSecret].length : undefined;
  }
  export(options) {
    if (this._type === "secret") {
      const format = options?.format;
      if (format === undefined || format === "buffer") return Buffer.from(this[kSecret]);
      if (format === "jwk") return { kty: "oct", k: Buffer.from(this[kSecret]).toString("base64url") };
      throw codeError(TypeError, `The property 'options.format' must be one of: undefined, 'buffer', 'jwk'. Received '${format}'`, "ERR_INVALID_ARG_VALUE");
    }
    if (options === undefined || options === null || typeof options !== "object") throw invalidArgType("options", "of type object", options);
    const format = options.format ?? "pem";
    if (format !== "pem" && format !== "der" && format !== "jwk") {
      throw codeError(TypeError, `The property 'options.format' must be one of: 'pem', 'der', 'jwk'. Received '${format}'`, "ERR_INVALID_ARG_VALUE");
    }
    const type = options.type ?? (this._type === "public" ? "spki" : "pkcs8");
    if (format !== "jwk" && options.type === undefined) {
      throw codeError(TypeError, `The property 'options.type' must be one of: ${this._type === "public" ? "'pkcs1', 'spki'" : "'pkcs1', 'pkcs8', 'sec1'"}. Received undefined`, "ERR_INVALID_ARG_VALUE");
    }
    const passphrase = options.passphrase === undefined ? undefined : toBytes(options.passphrase, undefined, "options.passphrase");
    const out = ops.keyExport(this[kHandle], { format, type, cipher: options.cipher, passphrase });
    if (format === "jwk") return JSON.parse(out.jwk);
    if (typeof out === "string") return out;
    return asBuffer(out);
  }
  equals(other) {
    if (!(other instanceof KeyObject)) throw invalidArgType("otherKeyObject", "an instance of KeyObject", other);
    if (other.type !== this.type) return false;
    const a = this._type === "secret" ? this[kSecret] : this[kHandle].der;
    const b = other._type === "secret" ? other[kSecret] : other[kHandle].der;
    return a.length === b.length && Buffer.from(a).equals(Buffer.from(b));
  }
  toCryptoKey(algorithm, extractable, usages) {
    return makeCryptoKey(this, normalizeAlgorithm(algorithm), extractable, usages);
  }
  static from(key) {
    if (!isCryptoKey(key)) throw invalidArgType("key", "an instance of CryptoKey", key);
    return key[kKeyObject];
  }
  get [Symbol.toStringTag]() {
    return "KeyObject";
  }
}

class SecretKeyObject extends KeyObject {}
class PublicKeyObject extends KeyObject {}
class PrivateKeyObject extends KeyObject {}

function keyFromHandle(handle) {
  return handle.type === "private" ? new PrivateKeyObject("private", handle) : new PublicKeyObject("public", handle);
}

function createSecretKey(key, encoding) {
  const bytes = typeof key === "string" ? Buffer.from(key, encoding ?? "utf8") : toBytes(key, encoding, "key");
  return new SecretKeyObject("secret", new Uint8Array(bytes));
}

function isKeyLike(v) {
  return typeof v === "string" || ArrayBuffer.isView(v) || v instanceof ArrayBuffer;
}

/** Import an asymmetric key from Node's many input shapes. */
function importAsymmetric(input, wantPrivate) {
  if (input instanceof KeyObject) {
    if (input.type === "secret") {
      throw codeError(TypeError, `Invalid key object type secret, expected ${wantPrivate ? "private" : "public"}.`, "ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE");
    }
    if (!wantPrivate && input.type === "private") return keyFromHandle(ops.keyPublic(input[kHandle]));
    if (wantPrivate && input.type === "public") {
      throw codeError(TypeError, "Invalid key object type public, expected private.", "ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE");
    }
    return input;
  }
  if (isCryptoKey(input)) return importAsymmetric(input[kKeyObject], wantPrivate);
  let key = input;
  let format = "pem";
  let type;
  let passphrase;
  let encoding;
  if (!isKeyLike(input)) {
    if (input === null || typeof input !== "object") {
      throw invalidArgType("key", "of type string or an instance of ArrayBuffer, Buffer, TypedArray, DataView, KeyObject, or CryptoKey", input);
    }
    key = input.key;
    if (key instanceof KeyObject || isCryptoKey(key)) return importAsymmetric(key, wantPrivate);
    format = input.format ?? "pem";
    type = input.type;
    passphrase = input.passphrase;
    encoding = input.encoding;
    if (format === "jwk") {
      if (key === null || typeof key !== "object") throw invalidArgType("key.key", "of type object", key);
      return keyFromHandle(ops.keyImportJwk(JSON.stringify(key), wantPrivate));
    }
  }
  const data = typeof key === "string" ? Buffer.from(key, format === "der" ? encoding ?? "utf8" : "utf8") : toBytes(key, undefined, "key");
  const pass = passphrase === undefined || passphrase === null ? undefined : toBytes(passphrase, undefined, "passphrase");
  return keyFromHandle(ops.keyImport(data, { format, type, passphrase: pass, private: wantPrivate }));
}

function createPrivateKey(key) {
  return importAsymmetric(key, true);
}

function createPublicKey(key) {
  return importAsymmetric(key, false);
}

/** Signature options carried by a key argument: `{ key, padding, saltLength, dsaEncoding }`. */
function keyOptions(input) {
  if (input && typeof input === "object" && !isKeyLike(input) && !(input instanceof KeyObject) && !isCryptoKey(input)) {
    return { padding: input.padding, saltLength: input.saltLength, dsaEncoding: input.dsaEncoding };
  }
  return {};
}

function signingKey(input, wantPrivate) {
  if (input instanceof KeyObject && input.type === "secret") {
    throw codeError(TypeError, `Invalid key object type secret, expected ${wantPrivate ? "private" : "public"}.`, "ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE");
  }
  if (!wantPrivate && input instanceof KeyObject) return input;
  if (!wantPrivate) {
    // verify() accepts private keys too (their public half is used).
    try {
      return importAsymmetric(input, false);
    } catch (err) {
      return importAsymmetric(input, true);
    }
  }
  return importAsymmetric(input, true);
}

// ---------------------------------------------------------------------------
// Sign / Verify
// ---------------------------------------------------------------------------

class Sign extends Writable {
  #alg;
  #chunks = [];
  constructor(alg, options) {
    super(options);
    this.#alg = checkDigest(alg, "sign");
  }
  update(data, encoding) {
    this.#chunks.push(Buffer.from(toBytes(data, encoding)));
    return this;
  }
  _write(chunk, encoding, cb) {
    this.update(chunk, encoding);
    cb();
  }
  sign(key, outputEncoding) {
    const k = signingKey(key, true);
    noEdDsaStream(k);
    const sig = ops.sign(this.#alg, k[kHandle], Buffer.concat(this.#chunks), keyOptions(key));
    return encodeOut(sig, outputEncoding);
  }
}

class Verify extends Writable {
  #alg;
  #chunks = [];
  constructor(alg, options) {
    super(options);
    this.#alg = checkDigest(alg, "sign");
  }
  update(data, encoding) {
    this.#chunks.push(Buffer.from(toBytes(data, encoding)));
    return this;
  }
  _write(chunk, encoding, cb) {
    this.update(chunk, encoding);
    cb();
  }
  verify(key, signature, signatureEncoding) {
    const k = signingKey(key, false);
    noEdDsaStream(k);
    return ops.verify(this.#alg, k[kHandle], Buffer.concat(this.#chunks), toBytes(signature, signatureEncoding, "signature"), keyOptions(key));
  }
}

/** EdDSA signs whole messages: Node's Sign/Verify streams refuse Ed25519/Ed448 keys. */
function noEdDsaStream(k) {
  const type = k.asymmetricKeyType;
  if (type === "ed25519" || type === "ed448") throw codeError(Error, "Unsupported crypto operation", "ERR_CRYPTO_UNSUPPORTED_OPERATION");
}

function oneShot(fn, callback) {
  if (typeof callback !== "function") return fn();
  let result;
  try {
    result = fn();
  } catch (err) {
    process.nextTick(callback, err);
    return;
  }
  process.nextTick(callback, null, result);
}

function sign(algorithm, data, key, callback) {
  const alg = algorithm === null || algorithm === undefined ? null : checkDigest(algorithm, "sign");
  return oneShot(() => {
    const k = signingKey(key, true);
    return asBuffer(ops.sign(alg, k[kHandle], toBytes(data), keyOptions(key)));
  }, callback);
}

function verify(algorithm, data, key, signature, callback) {
  const alg = algorithm === null || algorithm === undefined ? null : checkDigest(algorithm, "sign");
  return oneShot(() => {
    const k = signingKey(key, false);
    return ops.verify(alg, k[kHandle], toBytes(data), toBytes(signature, undefined, "signature"), keyOptions(key));
  }, callback);
}

// ---------------------------------------------------------------------------
// Key generation
// ---------------------------------------------------------------------------

function encodeGenerated(key, encoding) {
  if (!encoding) return key;
  return key.export(encoding);
}

function keyGenArgs(type, options = {}) {
  if (typeof type !== "string") throw invalidArgType("type", "of type string", type);
  const opts = { ...options };
  if (type === "rsa" || type === "rsa-pss" || type === "dsa") {
    validateUint32(opts.modulusLength, "options.modulusLength");
  }
  if (type === "dsa" && opts.divisorLength !== undefined && opts.divisorLength !== null) {
    validateInt32(opts.divisorLength, "options.divisorLength", 0);
  }
  if (opts.publicExponent === undefined) opts.publicExponent = 65537;
  return opts;
}

function generateKeyPairSync(type, options) {
  const opts = keyGenArgs(type, options);
  const [priv, pub] = ops.keyGenerate(type, opts, false);
  return {
    publicKey: encodeGenerated(keyFromHandle(pub), opts.publicKeyEncoding),
    privateKey: encodeGenerated(keyFromHandle(priv), opts.privateKeyEncoding),
  };
}

function generateKeyPair(type, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = undefined;
  }
  if (typeof callback !== "function") throw invalidArgType("callback", "of type function", callback);
  const opts = keyGenArgs(type, options);
  ops.keyGenerate(type, opts, true).then(
    ([priv, pub]) => {
      let publicKey, privateKey;
      try {
        publicKey = encodeGenerated(keyFromHandle(pub), opts.publicKeyEncoding);
        privateKey = encodeGenerated(keyFromHandle(priv), opts.privateKeyEncoding);
      } catch (err) {
        callback(err);
        return;
      }
      callback(null, publicKey, privateKey);
    },
    (err) => callback(err),
  );
}

function generateKeySync(type, options = {}) {
  const length = options.length;
  if (type === "hmac") {
    if (!Number.isInteger(length) || length < 8 || length > 2 ** 31 - 1) {
      throw codeError(RangeError, `The value of "options.length" is out of range. It must be >= 8 && <= 2147483647. Received ${length}`, "ERR_OUT_OF_RANGE");
    }
  } else if (type === "aes") {
    if (![128, 192, 256].includes(length)) {
      throw codeError(TypeError, `The property 'options.length' must be one of: 128, 192, 256. Received ${length}`, "ERR_INVALID_ARG_VALUE");
    }
  } else {
    throw codeError(TypeError, `The argument 'type' must be one of: 'aes', 'hmac'. Received '${type}'`, "ERR_INVALID_ARG_VALUE");
  }
  return createSecretKey(randomBytes(Math.floor(length / 8)));
}

function generateKey(type, options, callback) {
  oneShot(() => generateKeySync(type, options), callback);
}

// ---------------------------------------------------------------------------
// Key agreement
// ---------------------------------------------------------------------------

function curveName(name) {
  const map = { "P-256": "prime256v1", secp256r1: "prime256v1", "P-384": "secp384r1", "P-521": "secp521r1" };
  return map[name] ?? name;
}

class ECDH {
  #curve;
  #priv;
  #pub;
  constructor(curve) {
    if (typeof curve !== "string") throw invalidArgType("curve", "of type string", curve);
    this.#curve = curveName(curve);
    if (!curves.includes(this.#curve)) throw codeError(TypeError, "Invalid EC curve name", "ERR_CRYPTO_INVALID_CURVE");
  }
  static convertKey(key, curve, inputEncoding, outputEncoding, format) {
    const name = curveName(curve);
    if (!curves.includes(name)) throw codeError(TypeError, "Invalid EC curve name", "ERR_CRYPTO_INVALID_CURVE");
    const out = ops.ecdh("convert", name, toBytes(key, inputEncoding, "key"), new Uint8Array(0), format === "compressed");
    return encodeOut(out, outputEncoding);
  }
  generateKeys(encoding, format) {
    this.#priv = asBuffer(ops.ecdh("generate", this.#curve, new Uint8Array(0), new Uint8Array(0), false));
    this.#pub = asBuffer(ops.ecdh("public", this.#curve, this.#priv, new Uint8Array(0), false));
    return this.getPublicKey(encoding, format);
  }
  computeSecret(other, inputEncoding, outputEncoding) {
    if (!this.#priv) throw codeError(Error, "Failed to compute ECDH key", "ERR_CRYPTO_OPERATION_FAILED");
    const out = ops.ecdh("compute", this.#curve, this.#priv, toBytes(other, inputEncoding, "otherPublicKey"), false);
    return encodeOut(out, outputEncoding);
  }
  getPublicKey(encoding, format) {
    if (!this.#pub) throw codeError(Error, "Failed to get ECDH public key", "ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY");
    const pub = format === "compressed" ? asBuffer(ops.ecdh("convert", this.#curve, this.#pub, new Uint8Array(0), true)) : this.#pub;
    return encoding && encoding !== "buffer" ? pub.toString(encoding) : Buffer.from(pub);
  }
  getPrivateKey(encoding) {
    if (!this.#priv) throw codeError(Error, "Failed to get ECDH private key", "ERR_CRYPTO_INVALID_STATE");
    return encoding && encoding !== "buffer" ? this.#priv.toString(encoding) : Buffer.from(this.#priv);
  }
  setPrivateKey(key, encoding) {
    this.#priv = Buffer.from(toBytes(key, encoding, "privateKey"));
    this.#pub = asBuffer(ops.ecdh("public", this.#curve, this.#priv, new Uint8Array(0), false));
    return this;
  }
  setPublicKey(key, encoding) {
    this.#pub = asBuffer(ops.ecdh("convert", this.#curve, toBytes(key, encoding, "publicKey"), new Uint8Array(0), false));
    return this;
  }
}

const createECDH = (curve) => new ECDH(curve);

function bytesToBigInt(b) {
  return b.length ? BigInt(`0x${Buffer.from(b).toString("hex")}`) : 0n;
}

// ---------------------------------------------------------------------------
// Primes
// ---------------------------------------------------------------------------

function bigIntToBytes(n) {
  let hex = n.toString(16);
  if (hex.length % 2) hex = `0${hex}`;
  return Buffer.from(hex, "hex");
}

function validateInt32(value, name, min = -2147483648, max = 2147483647) {
  if (typeof value !== "number") throw invalidArgType(name, "of type number", value);
  if (!Number.isInteger(value)) throw codeError(RangeError, `The value of "${name}" is out of range. It must be an integer. Received ${value}`, "ERR_OUT_OF_RANGE");
  if (value < min || value > max) throw codeError(RangeError, `The value of "${name}" is out of range. It must be >= ${min} && <= ${max}. Received ${value}`, "ERR_OUT_OF_RANGE");
}

function validateFunction(fn, name) {
  if (typeof fn !== "function") throw invalidArgType(name, "of type function", fn);
}

function primeBytes(value, name) {
  if (typeof value === "bigint") {
    if (value < 0n) throw codeError(RangeError, `The value of "${name}" is out of range. It must be >= 0. Received ${value}n`, "ERR_OUT_OF_RANGE");
    return bigIntToBytes(value);
  }
  if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  if (value instanceof ArrayBuffer || (typeof SharedArrayBuffer !== "undefined" && value instanceof SharedArrayBuffer)) return new Uint8Array(value);
  throw invalidArgType(name, "of type bigint or an instance of ArrayBuffer, TypedArray, Buffer, or DataView", value);
}

function primeOptions(size, options) {
  validateInt32(size, "size", 1);
  if (options === undefined) options = {};
  if (options === null || typeof options !== "object") throw invalidArgType("options", "of type object", options);
  const { safe = false, bigint = false } = options;
  let { add, rem } = options;
  if (typeof safe !== "boolean") throw invalidArgType("options.safe", "of type boolean", safe);
  if (typeof bigint !== "boolean") throw invalidArgType("options.bigint", "of type boolean", bigint);
  if (add !== undefined) add = bytesToBigInt(primeBytes(add, "options.add"));
  if (rem !== undefined) rem = bytesToBigInt(primeBytes(rem, "options.rem"));
  if (add !== undefined) {
    if (add.toString(2).length > size) throw codeError(RangeError, "invalid options.add", "ERR_OUT_OF_RANGE");
    if (rem !== undefined && add <= rem) throw codeError(RangeError, "invalid options.rem", "ERR_OUT_OF_RANGE");
  }
  return [size, safe, add === undefined ? null : bigIntToBytes(add), add === undefined || rem === undefined ? null : bigIntToBytes(rem), bigint];
}

function primeResult(bytes, bigint) {
  if (bigint) return bytesToBigInt(bytes);
  const out = new ArrayBuffer(bytes.byteLength);
  new Uint8Array(out).set(bytes);
  return out;
}

function generatePrimeSync(size, options) {
  const [bits, safe, add, rem, bigint] = primeOptions(size, options);
  return primeResult(ops.primeGenerate(bits, safe, add, rem, false), bigint);
}

function generatePrime(size, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = undefined;
  }
  validateFunction(callback, "callback");
  const [bits, safe, add, rem, bigint] = primeOptions(size, options);
  ops.primeGenerate(bits, safe, add, rem, true).then(
    (bytes) => callback(undefined, primeResult(bytes, bigint)),
    (err) => callback(err),
  );
}

function checkPrimeOptions(candidate, options) {
  const bytes = primeBytes(candidate, "candidate");
  if (options === undefined) options = {};
  if (options === null || typeof options !== "object") throw invalidArgType("options", "of type object", options);
  const { checks = 0 } = options;
  validateInt32(checks, "options.checks", 0);
  return [bytes, checks];
}

function checkPrimeSync(candidate, options) {
  const [bytes, checks] = checkPrimeOptions(candidate, options);
  return ops.primeCheck(bytes, checks, false)[0] === 1;
}

function checkPrime(candidate, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = undefined;
  }
  const [bytes, checks] = checkPrimeOptions(candidate, options);
  validateFunction(callback, "callback");
  ops.primeCheck(bytes, checks, true).then(
    (r) => callback(undefined, r[0] === 1),
    (err) => callback(err),
  );
}

const kKnownGroup = Symbol("kKnownGroup");

class DiffieHellman {
  #prime;
  #generator;
  #priv;
  #pub;
  constructor(sizeOrKey, keyEncoding, generator, genEncoding) {
    if (typeof sizeOrKey !== "number" && typeof sizeOrKey !== "string" && !ArrayBuffer.isView(sizeOrKey) && !(sizeOrKey instanceof ArrayBuffer)) {
      throw invalidArgType("sizeOrKey", "one of type number or string or an instance of ArrayBuffer, Buffer, TypedArray, or DataView", sizeOrKey);
    }
    if (typeof sizeOrKey === "number") validateInt32(sizeOrKey, "sizeOrKey");
    const knownGroup = genEncoding === kKnownGroup;
    if (knownGroup) genEncoding = undefined;
    if (keyEncoding && (typeof keyEncoding !== "string" || (!Buffer.isEncoding(keyEncoding) && keyEncoding !== "buffer"))) {
      genEncoding = generator;
      generator = keyEncoding;
      keyEncoding = false;
    }
    if (!generator) generator = 2;
    else if (typeof generator === "number") validateInt32(generator, "generator");
    else if (typeof generator === "string") generator = Buffer.from(generator, genEncoding && genEncoding !== "buffer" ? genEncoding : "utf8");
    else if (!ArrayBuffer.isView(generator) && !(generator instanceof ArrayBuffer)) {
      throw invalidArgType("generator", "one of type number or string or an instance of ArrayBuffer, Buffer, TypedArray, or DataView", generator);
    }
    const badGenerator = () => codeError(Error, "error:02800065:Diffie-Hellman routines::bad generator", "ERR_OSSL_DH_BAD_GENERATOR");
    let genBytes;
    if (typeof generator === "number") {
      if (generator < 2) throw badGenerator();
      genBytes = bigIntToBytes(BigInt(generator));
    } else {
      genBytes = Buffer.from(toBytes(generator, undefined, "generator"));
      const g = bytesToBigInt(genBytes);
      if (g === 0n || g === 1n) throw badGenerator();
    }
    if (typeof sizeOrKey === "number") {
      if (sizeOrKey < 2) throw codeError(Error, "error:0280007E:Diffie-Hellman routines::modulus too small", "ERR_OSSL_DH_MODULUS_TOO_SMALL");
      if (sizeOrKey < 512) throw codeError(TypeError, "Invalid DH parameters", "ERR_INVALID_ARG_VALUE");
      this.#prime = asBuffer(ops.dhGenerate(sizeOrKey, Number(bytesToBigInt(genBytes))));
    } else {
      this.#prime = Buffer.from(toBytes(sizeOrKey, keyEncoding, "sizeOrKey"));
    }
    this.#generator = genBytes;
    const verifyError = knownGroup ? 0 : ops.dhCheck(this.#prime, this.#generator);
    Object.defineProperty(this, "verifyError", { enumerable: true, value: verifyError, writable: true, configurable: true });
  }
  generateKeys(encoding) {
    if (this.#prime.length * 8 - Math.clz32(this.#prime[0] ?? 0) + 24 < 512) {
      throw codeError(Error, "Key generation failed", "ERR_CRYPTO_OPERATION_FAILED");
    }
    if (!this.#priv) {
      // A private exponent a bit shorter than the prime.
      this.#priv = randomBytes(Math.max(32, this.#prime.length - 1));
    }
    this.#pub = asBuffer(ops.modPow(this.#generator, this.#priv, this.#prime));
    return this.getPublicKey(encoding);
  }
  computeSecret(other, inputEncoding, outputEncoding) {
    const peer = toBytes(other, inputEncoding, "otherPublicKey");
    const p = bytesToBigInt(this.#prime);
    const y = bytesToBigInt(peer);
    if (y <= 1n || y >= p - 1n) throw codeError(Error, "Supplied key is too small", "ERR_CRYPTO_INVALID_KEYLEN");
    return encodeOut(ops.modPow(peer, this.#priv, this.#prime), outputEncoding);
  }
  getPrime(encoding) {
    return encodeOut(this.#prime, encoding);
  }
  getGenerator(encoding) {
    return encodeOut(this.#generator, encoding);
  }
  getPublicKey(encoding) {
    return encodeOut(this.#pub, encoding);
  }
  getPrivateKey(encoding) {
    return encodeOut(this.#priv, encoding);
  }
  setPublicKey(key, encoding) {
    this.#pub = Buffer.from(toBytes(key, encoding, "publicKey"));
  }
  setPrivateKey(key, encoding) {
    this.#priv = Buffer.from(toBytes(key, encoding, "privateKey"));
  }
}

function createDiffieHellman(prime, primeEncoding, generator, generatorEncoding) {
  return new DiffieHellman(prime, primeEncoding, generator, generatorEncoding);
}

function getDiffieHellman(name) {
  const prime = ops.dhGroup(String(name));
  if (!prime) throw codeError(Error, "Unknown DH group", "ERR_CRYPTO_UNKNOWN_DH_GROUP");
  const dh = new DiffieHellman(prime, undefined, 2, kKnownGroup);
  dh.setPublicKey = undefined;
  dh.setPrivateKey = undefined;
  return dh;
}

function diffieHellman(options, callback) {
  if (options === null || typeof options !== "object") throw invalidArgType("options", "of type object", options);
  const { privateKey, publicKey } = options;
  if (!(privateKey instanceof KeyObject)) throw invalidArgType("options.privateKey", "an instance of KeyObject", privateKey);
  if (!(publicKey instanceof KeyObject)) throw invalidArgType("options.publicKey", "an instance of KeyObject", publicKey);
  return oneShot(() => asBuffer(ops.deriveSecret(privateKey[kHandle], publicKey[kHandle])), callback);
}

// ---------------------------------------------------------------------------
// RSA encryption
// ---------------------------------------------------------------------------

const constants = {
  OPENSSL_VERSION_NUMBER: 810549312, SSL_OP_ALL: 2147485776, SSL_OP_NO_SSLv2: 0, SSL_OP_NO_SSLv3: 33554432,
  SSL_OP_NO_TLSv1: 67108864, SSL_OP_NO_TLSv1_1: 268435456, SSL_OP_NO_TLSv1_2: 134217728, SSL_OP_NO_TLSv1_3: 536870912,
  SSL_OP_NO_TICKET: 16384, SSL_OP_NO_COMPRESSION: 131072, SSL_OP_CIPHER_SERVER_PREFERENCE: 4194304,
  DH_CHECK_P_NOT_SAFE_PRIME: 2, DH_CHECK_P_NOT_PRIME: 1, DH_UNABLE_TO_CHECK_GENERATOR: 4, DH_NOT_SUITABLE_GENERATOR: 8,
  RSA_PKCS1_PADDING: 1, RSA_NO_PADDING: 3, RSA_PKCS1_OAEP_PADDING: 4, RSA_X931_PADDING: 5, RSA_PKCS1_PSS_PADDING: 6,
  RSA_PSS_SALTLEN_DIGEST: -1, RSA_PSS_SALTLEN_MAX_SIGN: -2, RSA_PSS_SALTLEN_AUTO: -2, TLS1_VERSION: 769,
  TLS1_1_VERSION: 770, TLS1_2_VERSION: 771, TLS1_3_VERSION: 772, POINT_CONVERSION_COMPRESSED: 2,
  POINT_CONVERSION_UNCOMPRESSED: 4, POINT_CONVERSION_HYBRID: 6,
  defaultCoreCipherList: "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384",
  defaultCipherList: "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384",
};

function rsaCrypt(op, defaultPadding, wantPrivate) {
  return (key, buffer) => {
    const k = wantPrivate ? importAsymmetric(key, true) : (() => {
      try {
        return importAsymmetric(key, false);
      } catch {
        return importAsymmetric(key, true);
      }
    })();
    const opts = key && typeof key === "object" && !isKeyLike(key) && !(key instanceof KeyObject) ? key : {};
    const handle = op === "publicEncrypt" || op === "publicDecrypt" ? k[kHandle] : k[kHandle];
    const label = opts.oaepLabel === undefined ? undefined : toBytes(opts.oaepLabel, undefined, "oaepLabel");
    return asBuffer(ops.rsaCrypt(op, handle, toBytes(buffer, opts.encoding, "buffer"), opts.padding ?? defaultPadding, opts.oaepHash ?? "sha1", label));
  };
}

const publicEncrypt = rsaCrypt("publicEncrypt", constants.RSA_PKCS1_OAEP_PADDING, false);
const privateDecrypt = rsaCrypt("privateDecrypt", constants.RSA_PKCS1_OAEP_PADDING, true);
const privateEncrypt = rsaCrypt("privateEncrypt", constants.RSA_PKCS1_PADDING, true);
const publicDecrypt = rsaCrypt("publicDecrypt", constants.RSA_PKCS1_PADDING, false);

// ---------------------------------------------------------------------------
// KDFs and randomness
// ---------------------------------------------------------------------------

function checkKeylen(keylen) {
  if (typeof keylen !== "number") throw invalidArgType("keylen", "of type number", keylen);
  if (!Number.isInteger(keylen) || keylen < 0 || keylen > 2147483647) {
    throw codeError(RangeError, `The value of "keylen" is out of range. It must be >= 0 && <= 2147483647. Received ${keylen}`, "ERR_OUT_OF_RANGE");
  }
}

function pbkdf2Sync(password, salt, iterations, keylen, digest) {
  checkKeylen(keylen);
  if (typeof digest !== "string") throw invalidArgType("digest", "of type string", digest);
  if (!Number.isInteger(iterations) || iterations < 1) {
    throw codeError(RangeError, `The value of "iterations" is out of range. It must be >= 1 && <= 2147483647. Received ${iterations}`, "ERR_OUT_OF_RANGE");
  }
  checkDigest(digest, "hmac");
  return asBuffer(ops.pbkdf2(toBytes(password, undefined, "password"), toBytes(salt, undefined, "salt"), iterations, keylen, digest, false));
}

function pbkdf2(password, salt, iterations, keylen, digest, callback) {
  if (typeof digest === "function") {
    callback = digest;
    digest = undefined;
  }
  if (typeof callback !== "function") throw invalidArgType("callback", "of type function", callback);
  checkKeylen(keylen);
  if (typeof digest !== "string") throw invalidArgType("digest", "of type string", digest);
  checkDigest(digest, "hmac");
  ops.pbkdf2(toBytes(password, undefined, "password"), toBytes(salt, undefined, "salt"), iterations, keylen, digest, true).then(
    (out) => callback(null, asBuffer(out)),
    (err) => callback(err),
  );
}

function scryptArgs(options = {}) {
  const N = options.N ?? options.cost ?? 16384;
  const r = options.r ?? options.blockSize ?? 8;
  const p = options.p ?? options.parallelization ?? 1;
  const maxmem = options.maxmem ?? 32 * 1024 * 1024;
  return [N, r, p, maxmem];
}

function scryptSync(password, salt, keylen, options) {
  checkKeylen(keylen);
  return asBuffer(ops.scrypt(toBytes(password, undefined, "password"), toBytes(salt, undefined, "salt"), keylen, ...scryptArgs(options), false));
}

function scrypt(password, salt, keylen, options, callback) {
  if (typeof options === "function") {
    callback = options;
    options = undefined;
  }
  if (typeof callback !== "function") throw invalidArgType("callback", "of type function", callback);
  checkKeylen(keylen);
  let promise;
  try {
    promise = ops.scrypt(toBytes(password, undefined, "password"), toBytes(salt, undefined, "salt"), keylen, ...scryptArgs(options), true);
  } catch (err) {
    process.nextTick(callback, err);
    return;
  }
  promise.then((out) => callback(null, asBuffer(out)), (err) => callback(err));
}

function hkdfKey(key) {
  if (key instanceof KeyObject) return key[kSecret];
  return toBytes(key, undefined, "ikm");
}

function hkdfSync(digest, key, salt, info, keylen) {
  checkDigest(digest, "hmac");
  const out = ops.hkdf(digest, hkdfKey(key), toBytes(salt, undefined, "salt"), toBytes(info, undefined, "info"), keylen, false);
  return out.buffer.slice(out.byteOffset, out.byteOffset + out.byteLength);
}

function hkdf(digest, key, salt, info, keylen, callback) {
  if (typeof callback !== "function") throw invalidArgType("callback", "of type function", callback);
  checkDigest(digest, "hmac");
  ops.hkdf(digest, hkdfKey(key), toBytes(salt, undefined, "salt"), toBytes(info, undefined, "info"), keylen, true).then(
    (out) => callback(null, out.buffer.slice(out.byteOffset, out.byteOffset + out.byteLength)),
    (err) => callback(err),
  );
}

function randomBytes(size, cb) {
  if (typeof size !== "number") throw invalidArgType("size", "of type number", size);
  const u = ops.randomBytes(size);
  const b = asBuffer(u);
  if (typeof cb === "function") {
    process.nextTick(cb, null, b);
    return;
  }
  return b;
}

function randomFillSync(buf, offset = 0, size) {
  const view = buf instanceof ArrayBuffer ? new Uint8Array(buf) : new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
  const end = size === undefined ? view.length : offset + size;
  if (offset < 0 || end > view.length) {
    throw codeError(RangeError, `The value of "size + offset" is out of range. It must be <= ${view.length}. Received ${end}`, "ERR_OUT_OF_RANGE");
  }
  for (let pos = offset; pos < end; pos += 65536) ops.fillRandom(view.subarray(pos, Math.min(end, pos + 65536)));
  return buf;
}

function randomFill(buf, offset, size, cb) {
  if (typeof offset === "function") {
    cb = offset;
    offset = 0;
    size = undefined;
  } else if (typeof size === "function") {
    cb = size;
    size = undefined;
  }
  randomFillSync(buf, offset, size);
  process.nextTick(cb, null, buf);
}

function randomInt(min, max, cb) {
  if (max === undefined || typeof max === "function") {
    cb = max;
    max = min;
    min = 0;
  }
  if (!Number.isSafeInteger(min)) throw invalidArgType("min", "a safe integer", min);
  if (!Number.isSafeInteger(max)) throw invalidArgType("max", "a safe integer", max);
  const range = max - min;
  if (!(range > 0)) {
    throw codeError(RangeError, `The value of "max" is out of range. It must be greater than the value of "min" (${min}). Received ${max}`, "ERR_OUT_OF_RANGE");
  }
  if (range > 2 ** 48 - 1) {
    throw codeError(RangeError, `The value of "max - min" is out of range. It must be <= 281474976710655. Received ${range}`, "ERR_OUT_OF_RANGE");
  }
  // Rejection sampling over 48-bit values (no modulo bias).
  const limit = 2 ** 48 - (2 ** 48 % range);
  let n;
  do {
    const buf = ops.randomBytes(6);
    n = 0;
    for (const b of buf) n = n * 256 + b;
  } while (n >= limit);
  const v = min + (n % range);
  if (typeof cb === "function") {
    process.nextTick(cb, null, v);
    return;
  }
  return v;
}

function timingSafeEqual(a, b) {
  const x = toBytes(a, undefined, "buf1");
  const y = toBytes(b, undefined, "buf2");
  if (x.byteLength !== y.byteLength) throw codeError(RangeError, "Input buffers must have the same byte length", "ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH");
  let diff = 0;
  for (let i = 0; i < x.length; i++) diff |= x[i] ^ y[i];
  return diff === 0;
}

// ---------------------------------------------------------------------------
// X509Certificate (read-only subset)
// ---------------------------------------------------------------------------

function formatName(name) {
  return Object.entries(name).map(([k, v]) => `${k}=${v}`).join("\n");
}

function asn1Time(raw) {
  // UTCTime YYMMDDHHMMSSZ or GeneralizedTime YYYYMMDDHHMMSSZ
  const m = /^(\d{2}|\d{4})(\d{2})(\d{2})(\d{2})(\d{2})(\d{2})Z$/.exec(raw);
  if (!m) return new Date(NaN);
  let year = Number(m[1]);
  if (m[1].length === 2) year += year >= 50 ? 1900 : 2000;
  return new Date(Date.UTC(year, Number(m[2]) - 1, Number(m[3]), Number(m[4]), Number(m[5]), Number(m[6])));
}

class X509Certificate {
  #der;
  #info;
  constructor(buffer) {
    const bytes = toBytes(buffer, undefined, "buffer");
    const text = Buffer.from(bytes).toString("latin1");
    if (text.includes("-----BEGIN")) {
      const block = ops.pemDecode(text).find((b) => b.label.includes("CERTIFICATE"));
      if (!block) throw codeError(Error, "error:0680007B:asn1 encoding routines::header too long", "ERR_OSSL_ASN1_HEADER_TOO_LONG");
      this.#der = asBuffer(block.der);
    } else {
      this.#der = Buffer.from(bytes);
    }
    this.#info = ops.certInfo(this.#der);
    if (!this.#info) throw codeError(Error, "error:0680007B:asn1 encoding routines::header too long", "ERR_OSSL_ASN1_HEADER_TOO_LONG");
  }
  get raw() {
    return Buffer.from(this.#der);
  }
  get subject() {
    return formatName(this.#info.subject);
  }
  get issuer() {
    return formatName(this.#info.issuer);
  }
  get serialNumber() {
    return this.#info.serialNumber;
  }
  get validFromDate() {
    return asn1Time(this.#info.validFromRaw);
  }
  get validToDate() {
    return asn1Time(this.#info.validToRaw);
  }
  get validFrom() {
    return this.validFromDate.toUTCString().replace(/^\w+, (\d+) (\w+) (\d+) /, "$2 $1 $3 ").replace("GMT", "GMT");
  }
  get validTo() {
    return this.validToDate.toUTCString().replace(/^\w+, (\d+) (\w+) (\d+) /, "$2 $1 $3 ").replace("GMT", "GMT");
  }
  get fingerprint() {
    return fingerprint("sha1", this.#der);
  }
  get fingerprint256() {
    return fingerprint("sha256", this.#der);
  }
  get fingerprint512() {
    return fingerprint("sha512", this.#der);
  }
  get publicKey() {
    return keyFromHandle(ops.keyImport(asBuffer(this.#info.spki), { format: "der", type: "spki", private: false }));
  }
  get subjectAltName() {
    return this.#info.subjectaltname;
  }
  checkHost(name) {
    const cn = this.#info.subject.CN;
    if (!cn) return undefined;
    const pattern = new RegExp(`^${cn.replace(/\./g, "\\.").replace(/^\*/, "[^.]+")}$`, "i");
    return pattern.test(name) ? cn : undefined;
  }
  checkIssued(other) {
    return formatName(other.#info.subject) === formatName(this.#info.issuer);
  }
  toString() {
    return toPem("CERTIFICATE", this.#der);
  }
  toJSON() {
    return this.toString();
  }
  toLegacyObject() {
    return {
      subject: { ...this.#info.subject },
      issuer: { ...this.#info.issuer },
      subjectaltname: this.#info.subjectaltname,
      valid_from: this.validFrom,
      valid_to: this.validTo,
      serialNumber: this.serialNumber,
      fingerprint: this.fingerprint,
      fingerprint256: this.fingerprint256,
      fingerprint512: this.fingerprint512,
      raw: this.raw,
    };
  }
}

function fingerprint(alg, der) {
  return asBuffer(ops.hash(alg, der)).toString("hex").toUpperCase().match(/../g).join(":");
}

function toPem(label, der) {
  const b64 = Buffer.from(der).toString("base64");
  return `-----BEGIN ${label}-----\n${b64.match(/.{1,64}/g).join("\n")}\n-----END ${label}-----\n`;
}

// ---------------------------------------------------------------------------
// WebCrypto (node/webcrypto.js, loaded on first use)
// ---------------------------------------------------------------------------

const isCryptoKey = (v) => v !== null && typeof v === "object" && Object.prototype.hasOwnProperty.call(v, kKeyObject);
const loadWebCrypto = () => {
  internal.loadGlue("node/webcrypto.js");
  return internal.webcryptoImpl;
};
const makeCryptoKey = (...args) => loadWebCrypto().makeCryptoKey(...args);
const normalizeAlgorithm = (alg) => loadWebCrypto().normalizeAlgorithm(alg);
internal.cryptoCore = {
  kHandle, kSecret, kKeyObject, KeyObject, keyFromHandle, createSecretKey, importAsymmetric, generateKeyPairSync,
  privateDecrypt, publicEncrypt, createCipheriv, createDecipheriv, timingSafeEqual, randomBytes, constants, curveName,
  bytesToBigInt, asBuffer, codeError, isCryptoKey,
};

// ---------------------------------------------------------------------------
// Module
// ---------------------------------------------------------------------------

const crypto = {
  createHash: (alg, options) => new Hash(alg, options),
  createHmac: (alg, key, options) => new Hmac(alg, key, options),
  hash: (alg, data, encoding = "hex") => new Hash(alg).update(data).digest(encoding),
  getHashes: () => hashes.slice(),
  getCiphers: () => [...ops.ciphers()],
  getCipherInfo,
  getCurves: () => curves.slice(),
  createCipheriv,
  createDecipheriv,
  Cipheriv,
  Decipheriv,
  Cipher: Cipheriv,
  Decipher: Decipheriv,
  createSign: (alg, options) => new Sign(alg, options),
  createVerify: (alg, options) => new Verify(alg, options),
  Sign,
  Verify,
  sign,
  verify,
  createSecretKey,
  createPublicKey,
  createPrivateKey,
  KeyObject,
  generateKeyPair,
  generateKeyPairSync,
  generateKey,
  generateKeySync,
  createECDH,
  ECDH,
  createDiffieHellman,
  createDiffieHellmanGroup: getDiffieHellman,
  getDiffieHellman,
  DiffieHellman,
  DiffieHellmanGroup: getDiffieHellman,
  diffieHellman,
  publicEncrypt,
  privateDecrypt,
  privateEncrypt,
  publicDecrypt,
  pbkdf2,
  pbkdf2Sync,
  scrypt,
  scryptSync,
  hkdf,
  hkdfSync,
  randomBytes,
  pseudoRandomBytes: randomBytes,
  prng: randomBytes,
  rng: randomBytes,
  randomFillSync,
  randomFill,
  randomInt,
  randomUUID: () => ops.randomUUID(),
  getRandomValues: (a) => ops.fillRandom(a),
  timingSafeEqual,
  X509Certificate,
  Hash,
  Hmac,
  webcrypto: internal.webcrypto,
  subtle: internal.webcrypto.subtle,
  constants,
  getFips: () => 0,
  setFips() {},
  setEngine() {},
  secureHeapUsed: () => ({ total: 0, min: 0, used: 0, utilization: 0 }),
  generatePrime,
  generatePrimeSync,
  checkPrime,
  checkPrimeSync,
  _toPem: toPem,
  _isCryptoKey: isCryptoKey,
  _keyHandle: (k) => (k instanceof KeyObject ? (k._type === "secret" ? k[kSecret] : k[kHandle]) : undefined),
  _keyFromHandle: keyFromHandle,
};
internal.defineBuiltin("crypto", () => crypto);
