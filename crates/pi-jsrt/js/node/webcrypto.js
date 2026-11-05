// WebCrypto (crypto.subtle, CryptoKey) over node:crypto's key objects and ops. Loaded on the
// first use of a subtle method, `CryptoKey`, or KeyObject#toCryptoKey/KeyObject.from.

internal.requireBuiltin("crypto");
const {
  kHandle, kSecret, kKeyObject, KeyObject, keyFromHandle, createSecretKey, importAsymmetric, generateKeyPairSync,
  privateDecrypt, publicEncrypt, createCipheriv, createDecipheriv, timingSafeEqual, randomBytes, constants, curveName,
  bytesToBigInt, asBuffer, codeError,
} = internal.cryptoCore;
const Buffer = internal.Buffer;
const DOMException = internal.DOMException;


const webAlgNames = {
  "sha-1": "SHA-1", "sha-256": "SHA-256", "sha-384": "SHA-384", "sha-512": "SHA-512", hmac: "HMAC", "aes-gcm": "AES-GCM",
  "aes-cbc": "AES-CBC", "aes-ctr": "AES-CTR", "aes-kw": "AES-KW", ecdsa: "ECDSA", ecdh: "ECDH", ed25519: "Ed25519",
  x25519: "X25519", ed448: "Ed448", x448: "X448", "rsassa-pkcs1-v1_5": "RSASSA-PKCS1-v1_5", "rsa-pss": "RSA-PSS", "rsa-oaep": "RSA-OAEP", hkdf: "HKDF",
  pbkdf2: "PBKDF2",
};

const webError = (message, name) => new DOMException(message, name);

function normalizeAlgorithm(alg) {
  if (typeof alg === "string") alg = { name: alg };
  if (!alg || typeof alg !== "object" || typeof alg.name !== "string") throw webError("Unrecognized algorithm name", "NotSupportedError");
  const name = webAlgNames[alg.name.toLowerCase()];
  if (!name) throw webError("Unrecognized algorithm name", "NotSupportedError");
  if ((name === "Ed448" || name === "X448") && !warnedExperimental.has(name)) {
    warnedExperimental.add(name);
    process.emitWarning(`The ${name} Web Crypto API algorithm is an experimental feature and might change at any time`, "ExperimentalWarning");
  }
  const out = { ...alg, name };
  if (out.hash !== undefined) out.hash = normalizeAlgorithm(out.hash);
  return out;
}
const warnedExperimental = new Set();

const shaName = (h) => ({ "SHA-1": "sha1", "SHA-256": "sha256", "SHA-384": "sha384", "SHA-512": "sha512" })[h?.name ?? h];
const isCryptoKey = internal.cryptoCore.isCryptoKey;

class CryptoKey {
  constructor() {
    throw codeError(TypeError, "Illegal constructor", "ERR_ILLEGAL_CONSTRUCTOR");
  }
  get type() {
    return this[kKeyObject].type;
  }
  get extractable() {
    return this._extractable;
  }
  get algorithm() {
    return this._algorithm;
  }
  get usages() {
    return this._usages;
  }
  get [Symbol.toStringTag]() {
    return "CryptoKey";
  }
}

function makeCryptoKey(keyObject, algorithm, extractable, usages) {
  const key = Object.create(CryptoKey.prototype);
  Object.defineProperty(key, kKeyObject, { value: keyObject });
  Object.defineProperty(key, "_algorithm", { value: Object.freeze(algorithm) });
  Object.defineProperty(key, "_extractable", { value: !!extractable });
  Object.defineProperty(key, "_usages", { value: Object.freeze([...usages]) });
  return key;
}

function bufferSource(data, name = "data") {
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  throw codeError(TypeError, `Failed to execute: parameter ${name} is not of type 'BufferSource'`, "ERR_INVALID_ARG_TYPE");
}

const toArrayBuffer = (u8) => u8.buffer.slice(u8.byteOffset, u8.byteOffset + u8.byteLength);

function checkUsage(key, usage) {
  if (!isCryptoKey(key)) throw codeError(TypeError, "The \"key\" argument must be an instance of CryptoKey.", "ERR_INVALID_ARG_TYPE");
  if (!key.usages.includes(usage)) throw webError(`Unable to use this key to ${usage}`, "InvalidAccessError");
}

function checkAlgMatch(key, alg) {
  if (key.algorithm.name !== alg.name) throw webError("The requested operation is not valid for the provided key", "InvalidAccessError");
}

function namedCurveOf(keyObject) {
  const c = keyObject.asymmetricKeyDetails?.namedCurve;
  return { prime256v1: "P-256", secp384r1: "P-384", secp521r1: "P-521" }[c];
}

function rsaAlgorithm(name, keyObject, hash) {
  const d = keyObject.asymmetricKeyDetails;
  const e = d.publicExponent;
  const exp = [];
  for (let v = e; v > 0n; v >>= 8n) exp.unshift(Number(v & 0xffn));
  return { name, modulusLength: d.modulusLength, publicExponent: new Uint8Array(exp), hash: { name: hash.name ?? hash } };
}

function keyAlgorithmFor(alg, keyObject) {
  switch (alg.name) {
    case "HMAC":
      return { name: "HMAC", length: keyObject.symmetricKeySize * 8, hash: { name: alg.hash.name } };
    case "AES-GCM":
    case "AES-CBC":
    case "AES-CTR":
    case "AES-KW":
      return { name: alg.name, length: keyObject.symmetricKeySize * 8 };
    case "ECDSA":
    case "ECDH":
      return { name: alg.name, namedCurve: namedCurveOf(keyObject) ?? alg.namedCurve };
    case "Ed25519":
    case "X25519":
    case "Ed448":
    case "X448":
      return { name: alg.name };
    case "RSASSA-PKCS1-v1_5":
    case "RSA-PSS":
    case "RSA-OAEP":
      return rsaAlgorithm(alg.name, keyObject, alg.hash);
    default:
      return { name: alg.name };
  }
}

async function digest(algorithm, data) {
  const alg = normalizeAlgorithm(algorithm);
  const sha = shaName(alg.name);
  if (!sha) throw webError("Unrecognized algorithm name", "NotSupportedError");
  return toArrayBuffer(ops.hash(sha, bufferSource(data)));
}

const asymmetricUsages = {
  ECDSA: [["sign"], ["verify"]],
  Ed25519: [["sign"], ["verify"]],
  Ed448: [["sign"], ["verify"]],
  "RSASSA-PKCS1-v1_5": [["sign"], ["verify"]],
  "RSA-PSS": [["sign"], ["verify"]],
  "RSA-OAEP": [["decrypt", "unwrapKey"], ["encrypt", "wrapKey"]],
  ECDH: [["deriveKey", "deriveBits"], []],
  X25519: [["deriveKey", "deriveBits"], []],
  X448: [["deriveKey", "deriveBits"], []],
};

async function generateKeyWeb(algorithm, extractable, usages) {
  const alg = normalizeAlgorithm(algorithm);
  switch (alg.name) {
    case "HMAC": {
      const length = alg.length ?? { sha1: 512, sha256: 512, sha384: 1024, sha512: 1024 }[shaName(alg.hash)];
      const k = createSecretKey(randomBytes(Math.ceil(length / 8)));
      return makeCryptoKey(k, keyAlgorithmFor(alg, k), extractable, usages);
    }
    case "AES-GCM":
    case "AES-CBC":
    case "AES-CTR":
    case "AES-KW": {
      if (![128, 192, 256].includes(alg.length)) throw webError("AES key length must be 128, 192, or 256 bits", "OperationError");
      const k = createSecretKey(randomBytes(alg.length / 8));
      return makeCryptoKey(k, keyAlgorithmFor(alg, k), extractable, usages);
    }
    default: {
      const allowed = asymmetricUsages[alg.name];
      if (!allowed) throw webError("Unrecognized algorithm name", "NotSupportedError");
      let pair;
      if (alg.name.startsWith("RSA")) {
        const e = alg.publicExponent ? Number(bytesToBigInt(alg.publicExponent)) : 65537;
        pair = generateKeyPairSync("rsa", { modulusLength: alg.modulusLength, publicExponent: e });
      } else if (alg.name === "ECDSA" || alg.name === "ECDH") {
        pair = generateKeyPairSync("ec", { namedCurve: curveName(alg.namedCurve) });
      } else {
        pair = generateKeyPairSync(alg.name.toLowerCase());
      }
      const privateUsages = usages.filter((u) => allowed[0].includes(u));
      const publicUsages = usages.filter((u) => allowed[1].includes(u));
      return {
        privateKey: makeCryptoKey(pair.privateKey, keyAlgorithmFor(alg, pair.privateKey), extractable, privateUsages),
        publicKey: makeCryptoKey(pair.publicKey, keyAlgorithmFor(alg, pair.publicKey), true, publicUsages),
      };
    }
  }
}

async function importKeyWeb(format, keyData, algorithm, extractable, usages) {
  const alg = normalizeAlgorithm(algorithm);
  let keyObject;
  const symmetric = ["HMAC", "AES-GCM", "AES-CBC", "AES-CTR", "AES-KW", "HKDF", "PBKDF2"].includes(alg.name);
  if (symmetric) {
    if (format === "raw") keyObject = createSecretKey(bufferSource(keyData, "keyData"));
    else if (format === "jwk") {
      if (keyData.kty !== "oct") throw webError("Invalid JWK \"kty\" Parameter", "DataError");
      keyObject = createSecretKey(Buffer.from(keyData.k, "base64url"));
    } else throw webError(`Unable to import ${alg.name} key with format ${format}`, "NotSupportedError");
  } else if (format === "raw") {
    const asym = { ECDSA: "ec", ECDH: "ec", Ed25519: "ed25519", X25519: "x25519", Ed448: "ed448", X448: "x448" }[alg.name];
    if (!asym) throw webError(`Unable to import ${alg.name} key with format raw`, "NotSupportedError");
    try {
      keyObject = keyFromHandle(ops.keyImportRaw(asym, alg.namedCurve ? curveName(alg.namedCurve) : null, bufferSource(keyData, "keyData")));
    } catch (err) {
      throw webError("Invalid keyData", "DataError");
    }
  } else if (format === "jwk") {
    try {
      keyObject = importAsymmetric({ key: keyData, format: "jwk" }, keyData.d !== undefined);
    } catch (err) {
      throw webError(err.message, "DataError");
    }
  } else if (format === "spki" || format === "pkcs8") {
    try {
      keyObject = importAsymmetric({ key: Buffer.from(bufferSource(keyData, "keyData")), format: "der", type: format }, format === "pkcs8");
    } catch (err) {
      throw webError("Invalid keyData", "DataError");
    }
  } else {
    throw webError(`Unable to import key with format ${format}`, "NotSupportedError");
  }
  if (alg.name === "HMAC" && alg.length !== undefined && alg.length > keyObject.symmetricKeySize * 8) {
    throw webError("Invalid key length", "DataError");
  }
  return makeCryptoKey(keyObject, keyAlgorithmFor(alg, keyObject), extractable, usages);
}

async function exportKeyWeb(format, key) {
  if (!isCryptoKey(key)) throw codeError(TypeError, "The \"key\" argument must be an instance of CryptoKey.", "ERR_INVALID_ARG_TYPE");
  if (!key.extractable) throw webError("key is not extractable", "InvalidAccessError");
  const k = key[kKeyObject];
  if (k.type === "secret") {
    if (format === "raw") return toArrayBuffer(k[kSecret]);
    if (format === "jwk") {
      const jwk = { kty: "oct", k: Buffer.from(k[kSecret]).toString("base64url"), key_ops: [...key.usages], ext: true };
      const alg = key.algorithm;
      if (alg.name === "HMAC") jwk.alg = `HS${alg.hash.name.slice(4)}`;
      else if (alg.name.startsWith("AES")) jwk.alg = `A${alg.length}${alg.name.slice(4)}`;
      return jwk;
    }
    throw webError(`Unable to export a raw ${key.algorithm.name} key with format ${format}`, "NotSupportedError");
  }
  if (format === "raw") {
    if (k.type !== "public") throw webError("Unable to export a raw private key", "InvalidAccessError");
    return toArrayBuffer(ops.keyRaw(k[kHandle], false));
  }
  if (format === "spki" || format === "pkcs8") {
    if ((format === "spki") !== (k.type === "public")) throw webError("Invalid key type for format", "InvalidAccessError");
    return toArrayBuffer(new Uint8Array(k.export({ format: "der", type: format })));
  }
  if (format === "jwk") {
    const jwk = k.export({ format: "jwk" });
    jwk.key_ops = [...key.usages];
    jwk.ext = true;
    return jwk;
  }
  throw webError(`Unable to export key with format ${format}`, "NotSupportedError");
}

function aesCipherName(key, mode) {
  return `aes-${key.algorithm.length}-${mode}`;
}

async function encryptWeb(algorithm, key, data, decrypt = false) {
  const alg = normalizeAlgorithm(algorithm);
  checkUsage(key, decrypt ? "decrypt" : "encrypt");
  checkAlgMatch(key, alg);
  const input = bufferSource(data);
  const secret = key[kKeyObject][kSecret];
  try {
    switch (alg.name) {
      case "AES-GCM": {
        const tagLength = (alg.tagLength ?? 128) / 8;
        const iv = bufferSource(alg.iv, "iv");
        const c = (decrypt ? createDecipheriv : createCipheriv)(aesCipherName(key, "gcm"), secret, iv, { authTagLength: tagLength });
        if (alg.additionalData !== undefined) c.setAAD(bufferSource(alg.additionalData, "additionalData"));
        if (decrypt) {
          if (input.length < tagLength) throw webError("The provided data is too small.", "OperationError");
          c.setAuthTag(input.subarray(input.length - tagLength));
          const out = Buffer.concat([c.update(input.subarray(0, input.length - tagLength)), c.final()]);
          return toArrayBuffer(out);
        }
        const out = Buffer.concat([c.update(input), c.final(), c.getAuthTag()]);
        return toArrayBuffer(out);
      }
      case "AES-CBC": {
        const c = (decrypt ? createDecipheriv : createCipheriv)(aesCipherName(key, "cbc"), secret, bufferSource(alg.iv, "iv"));
        return toArrayBuffer(Buffer.concat([c.update(input), c.final()]));
      }
      case "AES-CTR": {
        const c = (decrypt ? createDecipheriv : createCipheriv)(aesCipherName(key, "ctr"), secret, bufferSource(alg.counter, "counter"));
        return toArrayBuffer(Buffer.concat([c.update(input), c.final()]));
      }
      case "RSA-OAEP": {
        const k = key[kKeyObject];
        const opts = { key: k, padding: constants.RSA_PKCS1_OAEP_PADDING, oaepHash: shaName(key.algorithm.hash) };
        if (alg.label !== undefined) opts.oaepLabel = bufferSource(alg.label, "label");
        return toArrayBuffer(decrypt ? privateDecrypt(opts, input) : publicEncrypt(opts, input));
      }
      default:
        throw webError("Unrecognized algorithm name", "NotSupportedError");
    }
  } catch (err) {
    if (err instanceof DOMException) throw err;
    throw webError(decrypt ? "The operation failed for an operation-specific reason" : err.message, "OperationError");
  }
}

function webSignOptions(alg, key) {
  switch (alg.name) {
    case "ECDSA":
      return { digest: shaName(alg.hash), opts: { dsaEncoding: "ieee-p1363" } };
    case "RSA-PSS":
      return { digest: shaName(key.algorithm.hash), opts: { padding: constants.RSA_PKCS1_PSS_PADDING, saltLength: alg.saltLength } };
    case "RSASSA-PKCS1-v1_5":
      return { digest: shaName(key.algorithm.hash), opts: { padding: constants.RSA_PKCS1_PADDING } };
    case "Ed25519":
      return { digest: null, opts: {} };
    case "Ed448":
      if (alg.context !== undefined && bufferSource(alg.context, "context").byteLength !== 0) {
        throw webError("Non zero-length context is not supported.", "NotSupportedError");
      }
      return { digest: null, opts: {} };
    default:
      throw webError("Unrecognized algorithm name", "NotSupportedError");
  }
}

async function signWeb(algorithm, key, data) {
  const alg = normalizeAlgorithm(algorithm);
  checkUsage(key, "sign");
  checkAlgMatch(key, alg);
  const input = bufferSource(data);
  if (alg.name === "HMAC") return toArrayBuffer(ops.hmac(shaName(key.algorithm.hash), key[kKeyObject][kSecret], input));
  const { digest: d, opts } = webSignOptions(alg, key);
  return toArrayBuffer(ops.sign(d, key[kKeyObject][kHandle], input, opts));
}

async function verifyWeb(algorithm, key, signature, data) {
  const alg = normalizeAlgorithm(algorithm);
  checkUsage(key, "verify");
  checkAlgMatch(key, alg);
  const input = bufferSource(data);
  const sig = bufferSource(signature, "signature");
  if (alg.name === "HMAC") {
    const mac = ops.hmac(shaName(key.algorithm.hash), key[kKeyObject][kSecret], input);
    return mac.length === sig.length && timingSafeEqual(mac, sig);
  }
  const { digest: d, opts } = webSignOptions(alg, key);
  return ops.verify(d, key[kKeyObject][kHandle], input, sig, opts);
}

async function deriveBitsWeb(algorithm, baseKey, length) {
  const alg = normalizeAlgorithm(algorithm);
  if (!isCryptoKey(baseKey)) throw codeError(TypeError, "The \"baseKey\" argument must be an instance of CryptoKey.", "ERR_INVALID_ARG_TYPE");
  if (!baseKey.usages.includes("deriveBits") && !baseKey.usages.includes("deriveKey")) throw webError("baseKey does not have deriveBits usage", "InvalidAccessError");
  checkAlgMatch(baseKey, alg);
  let bits;
  switch (alg.name) {
    case "ECDH":
    case "X25519":
    case "X448": {
      if (!isCryptoKey(alg.public)) throw codeError(TypeError, "algorithm.public must be a CryptoKey", "ERR_INVALID_ARG_TYPE");
      try {
        bits = asBuffer(ops.deriveSecret(baseKey[kKeyObject][kHandle], alg.public[kKeyObject][kHandle]));
      } catch (err) {
        throw webError(err.message, "OperationError");
      }
      break;
    }
    case "HKDF":
      if (length === null || length === undefined) throw webError("length cannot be null", "OperationError");
      bits = asBuffer(ops.hkdf(shaName(alg.hash), baseKey[kKeyObject][kSecret], bufferSource(alg.salt, "salt"), bufferSource(alg.info, "info"), Math.ceil(length / 8), false));
      break;
    case "PBKDF2":
      if (length === null || length === undefined) throw webError("length cannot be null", "OperationError");
      if (!(alg.iterations > 0)) throw webError("iterations cannot be zero", "OperationError");
      bits = asBuffer(await new Promise((resolve, reject) => ops.pbkdf2(baseKey[kKeyObject][kSecret], bufferSource(alg.salt, "salt"), alg.iterations, Math.ceil(length / 8), shaName(alg.hash), true).then(resolve, reject)));
      break;
    default:
      throw webError("Unrecognized algorithm name", "NotSupportedError");
  }
  if (length !== null && length !== undefined) {
    const bytes = Math.ceil(length / 8);
    if (bytes > bits.length) throw webError("derived bit length is too long", "OperationError");
    bits = Buffer.from(bits.subarray(0, bytes));
    if (length % 8) bits[bytes - 1] &= 0xff << (8 - (length % 8));
  }
  return toArrayBuffer(bits);
}

function derivedKeyLength(alg) {
  switch (alg.name) {
    case "AES-GCM":
    case "AES-CBC":
    case "AES-CTR":
    case "AES-KW":
      return alg.length;
    case "HMAC":
      return alg.length ?? { sha1: 512, sha256: 512, sha384: 1024, sha512: 1024 }[shaName(alg.hash)];
    default:
      throw webError("Unrecognized algorithm name", "NotSupportedError");
  }
}

async function deriveKeyWeb(algorithm, baseKey, derivedKeyAlgorithm, extractable, usages) {
  const target = normalizeAlgorithm(derivedKeyAlgorithm);
  const bits = await deriveBitsWeb(algorithm, baseKey, derivedKeyLength(target));
  return importKeyWeb("raw", bits, target, extractable, usages);
}

async function wrapKeyWeb(format, key, wrappingKey, wrapAlgorithm) {
  const alg = normalizeAlgorithm(wrapAlgorithm);
  checkUsage(wrappingKey, "wrapKey");
  let exported = await exportKeyWeb(format, key);
  if (format === "jwk") exported = new TextEncoder().encode(JSON.stringify(exported));
  const data = bufferSource(exported);
  if (alg.name === "AES-KW") return toArrayBuffer(ops.aesKw(wrappingKey[kKeyObject][kSecret], data, true));
  const tmp = makeCryptoKey(wrappingKey[kKeyObject], wrappingKey.algorithm, false, ["encrypt"]);
  return encryptWeb(alg, tmp, data, false);
}

async function unwrapKeyWeb(format, wrappedKey, unwrappingKey, unwrapAlgorithm, unwrappedKeyAlgorithm, extractable, usages) {
  const alg = normalizeAlgorithm(unwrapAlgorithm);
  checkUsage(unwrappingKey, "unwrapKey");
  let data;
  if (alg.name === "AES-KW") {
    try {
      data = ops.aesKw(unwrappingKey[kKeyObject][kSecret], bufferSource(wrappedKey), false);
    } catch (err) {
      throw webError(err.message, "OperationError");
    }
  } else {
    const tmp = makeCryptoKey(unwrappingKey[kKeyObject], unwrappingKey.algorithm, false, ["decrypt"]);
    data = new Uint8Array(await encryptWeb(alg, tmp, wrappedKey, true));
  }
  const keyData = format === "jwk" ? JSON.parse(new TextDecoder().decode(data)) : data;
  return importKeyWeb(format, keyData, unwrappedKeyAlgorithm, extractable, usages);
}

const subtle = {
  digest,
  generateKey: generateKeyWeb,
  importKey: importKeyWeb,
  exportKey: exportKeyWeb,
  encrypt: (algorithm, key, data) => encryptWeb(algorithm, key, data, false),
  decrypt: (algorithm, key, data) => encryptWeb(algorithm, key, data, true),
  sign: signWeb,
  verify: verifyWeb,
  deriveBits: deriveBitsWeb,
  deriveKey: deriveKeyWeb,
  wrapKey: wrapKeyWeb,
  unwrapKey: unwrapKeyWeb,
};
// The global `crypto.subtle` (web/globals.js) forwards here.
internal.subtleImpl = subtle;
Object.assign(internal.webcrypto.subtle, subtle);
globalThis.CryptoKey = CryptoKey;
internal.webcryptoImpl = { makeCryptoKey, normalizeAlgorithm };

