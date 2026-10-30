//! Node modules backed by native Rust code: `node:zlib`, `node:crypto` (ciphers, keys,
//! signatures, KDFs, WebCrypto), `node:tls`/`https`, and the `WebAssembly` global.

use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::{Value, json};

async fn run(source: &str) -> Value {
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let ns = tokio::time::timeout(std::time::Duration::from_secs(60), rt.eval_module(source, "native-test.mjs"))
        .await
        .expect("timed out")
        .unwrap_or_else(|e| panic!("{e}\n{:?}", e.as_js().and_then(|j| j.stack.clone())));
    ns.get("default").await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zlib_one_shot_callback_promise_and_errors() {
    // Independently checked with Node's gzipSync on macOS; the existing fixture is Linux.
    let gzip_hex=if cfg!(target_os="macos") { "1f8b08000000000000134b4c4a0600c241243503000000" } else { "1f8b08000000000000034b4c4a0600c241243503000000" };
    let v = run(r#"
        import zlib from "node:zlib";
        import { promisify } from "node:util";
        const text = "hello hello hello hello zlib ".repeat(50);
        const roundTrips = {};
        for (const [c, d] of [["deflate", "inflate"], ["gzip", "gunzip"], ["deflateRaw", "inflateRaw"], ["gzip", "unzip"], ["deflate", "unzip"], ["brotliCompress", "brotliDecompress"]]) {
            const packed = zlib[`${c}Sync`](text);
            roundTrips[`${c}/${d}`] = zlib[`${d}Sync`](packed).toString() === text && packed.length < text.length;
        }
        const viaCallback = await new Promise((resolve, reject) => zlib.gzip(text, (err, buf) => err ? reject(err) : zlib.gunzip(buf, (e, out) => e ? reject(e) : resolve(out.toString()))));
        const viaPromise = (await promisify(zlib.inflate)(await promisify(zlib.deflate)(Buffer.from("promised")))).toString();
        const err = (f) => { try { f(); return "no error"; } catch (e) { return [e.message, e.code, e.errno]; } };
        const cbErr = await new Promise((resolve) => zlib.inflate(Buffer.from("nope"), (e) => resolve(e.code)));
        export default {
            roundTrips, viaCallback: viaCallback === text, viaPromise,
            hex: zlib.deflateSync("abc").toString("hex"),
            gzipHex: zlib.gzipSync("abc").toString("hex"),
            badHeader: err(() => zlib.inflateSync(Buffer.from("hello"))),
            truncated: err(() => zlib.inflateSync(zlib.deflateSync("abc").subarray(0, 5))),
            badArg: err(() => zlib.gzipSync(42)),
            cbErr,
            crc: [zlib.crc32("hello"), zlib.crc32("world", zlib.crc32("hello "))],
            consts: [zlib.constants.Z_SYNC_FLUSH, zlib.constants.BROTLI_PARAM_QUALITY, zlib.Z_BEST_COMPRESSION],
        };
    "#)
    .await;
    assert_eq!(
        v,
        json!({
            "roundTrips": {"deflate/inflate": true, "gzip/gunzip": true, "deflateRaw/inflateRaw": true, "gzip/unzip": true, "deflate/unzip": true, "brotliCompress/brotliDecompress": true},
            "viaCallback": true, "viaPromise": "promised",
            "hex": "789c4b4c4a0600024d0127",
            "gzipHex": gzip_hex,
            "badHeader": ["incorrect header check", "Z_DATA_ERROR", -3],
            "truncated": ["unexpected end of file", "Z_BUF_ERROR", -5],
            "badArg": ["The \"buffer\" argument must be of type string or an instance of Buffer, TypedArray, DataView, or ArrayBuffer. Received type number (42)", "ERR_INVALID_ARG_TYPE", null],
            "cbErr": "Z_DATA_ERROR",
            "crc": [907060870, 222957957],
            "consts": [2, 1, 9],
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zlib_streams_flush_and_internal_handle() {
    let v = run(r#"
        import zlib from "node:zlib";
        import { pipeline } from "node:stream/promises";
        import { Readable, Writable } from "node:stream";
        const chunks = [];
        const big = Buffer.alloc(100000, "abcdefghij");
        await pipeline(Readable.from([big.subarray(0, 50000), big.subarray(50000)]), zlib.createGzip(), zlib.createGunzip({ chunkSize: 1024 }),
            new Writable({ write(c, e, cb) { chunks.push(c); cb(); } }));
        const out = Buffer.concat(chunks);
        // Sync flush: everything written so far becomes decodable.
        const deflate = zlib.createDeflate();
        const flushed = [];
        deflate.on("data", (d) => flushed.push(d));
        deflate.write("partial data");
        await new Promise((r) => deflate.flush(zlib.constants.Z_SYNC_FLUSH, r));
        const partial = zlib.inflateSync(Buffer.concat(flushed), { finishFlush: zlib.constants.Z_SYNC_FLUSH }).toString();
        // Error event on corrupt input.
        const streamErr = await new Promise((resolve) => {
            const g = zlib.createGunzip();
            g.on("error", (e) => resolve(e.message));
            g.end(Buffer.from("not gzip at all"));
        });
        // Node's internal handle as used by ssh2 (Z_PARTIAL_FLUSH, bounded output buffers).
        const Handle = zlib.createInflate()._handle.constructor;
        const state = new Uint32Array(2);
        const def = new Handle(zlib.constants.DEFLATE);
        def.init(15, -1, 8, 0, state, () => {}, undefined);
        const input = Buffer.from("ssh packet payload ".repeat(20));
        const packets = [];
        let off = 0;
        for (;;) {
            const outBuf = Buffer.alloc(16);
            def.writeSync(zlib.constants.Z_PARTIAL_FLUSH, input, off, off === 0 ? input.length : 0, outBuf, 0, 16);
            packets.push(outBuf.subarray(0, 16 - state[0]));
            if (state[0] !== 0) break;
            off = input.length;
        }
        const inf = new Handle(zlib.constants.INFLATE);
        inf.init(15, -1, 8, 0, state, () => {}, undefined);
        const compressed = Buffer.concat(packets);
        const outBuf = Buffer.alloc(1000);
        inf.writeSync(zlib.constants.Z_PARTIAL_FLUSH, compressed, 0, compressed.length, outBuf, 0, 1000);
        export default {
            same: out.equals(big), chunkMax: Math.max(...chunks.map((c) => c.length)),
            partial, streamErr,
            handle: outBuf.subarray(0, 1000 - state[0]).toString() === input.toString(),
            pieces: packets.length > 1,
        };
    "#)
    .await;
    assert_eq!(v, json!({ "same": true, "chunkMax": 1024, "partial": "partial data", "streamErr": "incorrect header check", "handle": true, "pieces": true }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crypto_ciphers_kdfs_and_errors() {
    let v = run(r#"
        import crypto from "node:crypto";
        const key = Buffer.from("0123456789abcdef");
        const iv = Buffer.from("fedcba9876543210");
        const enc = (alg, k, i, data, out = "hex") => { const c = crypto.createCipheriv(alg, k, i); return c.update(data, "utf8", out) + c.final(out); };
        const dec = (alg, k, i, data) => { const d = crypto.createDecipheriv(alg, k, i); return d.update(data, "hex", "utf8") + d.final("utf8"); };
        const cbc = enc("aes-128-cbc", key, iv, "hello world, this is a test");
        const gcmKey = crypto.randomBytes(32), gcmIv = crypto.randomBytes(12);
        const g = crypto.createCipheriv("aes-256-gcm", gcmKey, gcmIv);
        g.setAAD(Buffer.from("aad"));
        const gct = Buffer.concat([g.update("secret message"), g.final()]);
        const tag = g.getAuthTag();
        const gd = crypto.createDecipheriv("aes-256-gcm", gcmKey, gcmIv);
        gd.setAAD(Buffer.from("aad")); gd.setAuthTag(tag);
        const gplain = Buffer.concat([gd.update(gct), gd.final()]).toString();
        const err = (f) => { try { f(); return "no error"; } catch (e) { return [e.name, e.code ?? null, e.message]; } };
        const badTag = err(() => { const d = crypto.createDecipheriv("aes-256-gcm", gcmKey, gcmIv); d.setAuthTag(Buffer.alloc(16)); d.update(gct); d.final(); });
        const cc = crypto.createCipheriv("chacha20-poly1305", gcmKey, gcmIv, { authTagLength: 16 });
        const cct = Buffer.concat([cc.update("chacha"), cc.final()]);
        const ccd = crypto.createDecipheriv("chacha20-poly1305", gcmKey, gcmIv, { authTagLength: 16 });
        ccd.setAuthTag(cc.getAuthTag());
        const ccplain = Buffer.concat([ccd.update(cct), ccd.final()]).toString();
        const pbkdf2Async = await new Promise((r, j) => crypto.pbkdf2("pw", "salt", 1000, 16, "sha256", (e, k) => e ? j(e) : r(k.toString("hex"))));
        const scryptAsync = await new Promise((r, j) => crypto.scrypt("pw", "salt", 16, (e, k) => e ? j(e) : r(k.toString("hex"))));
        export default {
            cbc, cbcBack: dec("aes-128-cbc", key, iv, cbc),
            ctr: enc("aes-128-ctr", key, iv, "hello world, this is a test"),
            gplain, ccplain, badTag,
            unknown: err(() => crypto.createCipheriv("foo", key, iv)),
            keylen: err(() => crypto.createCipheriv("aes-128-cbc", Buffer.alloc(15), iv)),
            ivlen: err(() => crypto.createCipheriv("aes-128-cbc", key, Buffer.alloc(15))),
            badDecrypt: err(() => dec("aes-128-cbc", key, iv, "00".repeat(16))),
            hmacDigest: err(() => crypto.createHmac("foo", "k")),
            hashDigest: err(() => crypto.createHash("foo")),
            pbkdf2: crypto.pbkdf2Sync("pw", "salt", 1000, 16, "sha256").toString("hex"), pbkdf2Async,
            scrypt: crypto.scryptSync("pw", "salt", 16).toString("hex"), scryptAsync,
            hkdf: Buffer.from(crypto.hkdfSync("sha256", "key", "salt", "info", 16)).toString("hex"),
            hmac: crypto.createHmac("sha512-256", "k").update("x").digest("hex").length,
            info: crypto.getCipherInfo("aes-256-gcm"),
            ciphers: ["aes-128-gcm", "aes-256-ctr", "chacha20-poly1305", "aes-128-cbc"].every((c) => crypto.getCiphers().includes(c)),
            timing: err(() => crypto.timingSafeEqual(Buffer.alloc(1), Buffer.alloc(2))),
            randomInt: [...Array(50)].every(() => { const n = crypto.randomInt(3, 7); return n >= 3 && n < 7; }),
        };
    "#)
    .await;
    let err = |name: &str, code: &str, msg: &str| json!([name, code, msg]);
    assert_eq!(v["cbc"], json!("48a64c77eda317c2ece0b3eb1013593a9ddabef656d84916dd63e7bd9f8a76f4"));
    assert_eq!(v["cbcBack"], json!("hello world, this is a test"));
    assert_eq!(v["ctr"], json!("632eba1d99500866ca54a7faea6802543145854a8fc6adfb838b25"));
    assert_eq!(v["gplain"], json!("secret message"));
    assert_eq!(v["ccplain"], json!("chacha"));
    assert_eq!(v["badTag"], json!(["Error", null, "Unsupported state or unable to authenticate data"]));
    assert_eq!(v["unknown"], err("Error", "ERR_CRYPTO_UNKNOWN_CIPHER", "Unknown cipher"));
    assert_eq!(v["keylen"], err("RangeError", "ERR_CRYPTO_INVALID_KEYLEN", "Invalid key length"));
    assert_eq!(v["ivlen"], err("TypeError", "ERR_CRYPTO_INVALID_IV", "Invalid initialization vector"));
    assert_eq!(v["badDecrypt"], err("Error", "ERR_OSSL_BAD_DECRYPT", "error:1C800064:Provider routines::bad decrypt"));
    assert_eq!(v["hmacDigest"], err("TypeError", "ERR_CRYPTO_INVALID_DIGEST", "Invalid digest: foo"));
    assert_eq!(v["hashDigest"], json!(["Error", null, "Digest method not supported"]));
    assert_eq!(v["pbkdf2"], json!("0a38253555ce37f5c72a6b703f996814"));
    assert_eq!(v["pbkdf2Async"], v["pbkdf2"]);
    assert_eq!(v["scrypt"], json!("c0b515908e61334cae6d6003c00be60e"));
    assert_eq!(v["scryptAsync"], v["scrypt"]);
    assert_eq!(v["hkdf"], json!("9ca0d662557439e3b83365f2da4626d3"));
    assert_eq!(v["hmac"], json!(64));
    assert_eq!(v["info"], json!({"mode": "gcm", "name": "id-aes256-gcm", "nid": 901, "blockSize": 1, "ivLength": 12, "keyLength": 32}));
    assert_eq!(v["ciphers"], json!(true));
    assert_eq!(v["timing"], err("RangeError", "ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH", "Input buffers must have the same byte length"));
    assert_eq!(v["randomInt"], json!(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crypto_keys_signatures_and_key_agreement() {
    let v = run(r#"
        import crypto from "node:crypto";
        const out = {};
        // ssh2's Ed25519 probe.
        const edPem = "-----BEGIN PRIVATE KEY-----\r\nMC4CAQAwBQYDK2VwBCIEIHKj+sVa9WcD/q2DJUJaf43Kptc8xYuUQA4bOFj9vC8T\r\n-----END PRIVATE KEY-----";
        const edSig = crypto.sign(null, Buffer.from("a"), edPem);
        out.ed = [edSig.length, crypto.verify(null, Buffer.from("a"), edPem, edSig), crypto.createPrivateKey(edPem).asymmetricKeyType];
        // RSA: generate, PEM round trip, sign/verify (PKCS#1 + PSS), encrypt/decrypt.
        const { publicKey, privateKey } = crypto.generateKeyPairSync("rsa", { modulusLength: 1024, publicKeyEncoding: { type: "spki", format: "pem" }, privateKeyEncoding: { type: "pkcs8", format: "pem" } });
        out.pem = [publicKey.startsWith("-----BEGIN PUBLIC KEY-----"), privateKey.startsWith("-----BEGIN PRIVATE KEY-----")];
        const signer = crypto.createSign("RSA-SHA256");
        signer.update("payload");
        const sig = signer.sign(privateKey, "base64");
        out.rsaVerify = crypto.createVerify("sha256").update("payload").verify(publicKey, sig, "base64");
        const pss = crypto.sign("sha256", Buffer.from("x"), { key: privateKey, padding: crypto.constants.RSA_PKCS1_PSS_PADDING });
        out.pss = crypto.verify("sha256", Buffer.from("x"), { key: publicKey, padding: crypto.constants.RSA_PKCS1_PSS_PADDING }, pss);
        out.oaep = crypto.privateDecrypt(privateKey, crypto.publicEncrypt(publicKey, Buffer.from("secret"))).toString();
        const ko = crypto.createPublicKey(privateKey);
        out.details = [ko.type, ko.asymmetricKeyType, ko.asymmetricKeyDetails.modulusLength, String(ko.asymmetricKeyDetails.publicExponent)];
        const jwk = crypto.createPrivateKey(privateKey).export({ format: "jwk" });
        out.jwk = Object.keys(jwk);
        out.jwkBack = crypto.createPrivateKey({ key: jwk, format: "jwk" }).export({ type: "pkcs8", format: "pem" }) === privateKey;
        const pkcs1 = crypto.createPrivateKey(privateKey).export({ type: "pkcs1", format: "pem", cipher: "aes-256-cbc", passphrase: "pw" });
        out.encrypted = [pkcs1.includes("Proc-Type: 4,ENCRYPTED"), crypto.createPrivateKey({ key: pkcs1, passphrase: "pw" }).equals(crypto.createPrivateKey(privateKey))];
        try { crypto.createPrivateKey(pkcs1); } catch (e) { out.noPass = e.code; }
        // EC: async generation, ECDSA DER + P1363, ECDH, diffieHellman.
        const ec = await new Promise((r, j) => crypto.generateKeyPair("ec", { namedCurve: "P-384" }, (e, pub, priv) => e ? j(e) : r({ pub, priv })));
        const der = crypto.sign("sha384", Buffer.from("m"), ec.priv);
        const raw = crypto.sign("sha384", Buffer.from("m"), { key: ec.priv, dsaEncoding: "ieee-p1363" });
        out.ecdsa = [crypto.verify("sha384", Buffer.from("m"), ec.pub, der), crypto.verify("sha384", Buffer.from("m"), { key: ec.pub, dsaEncoding: "ieee-p1363" }, raw), raw.length, ec.pub.asymmetricKeyDetails.namedCurve];
        const a = crypto.createECDH("prime256v1"), b = crypto.createECDH("prime256v1");
        a.generateKeys(); b.generateKeys();
        out.ecdh = [a.computeSecret(b.getPublicKey()).equals(b.computeSecret(a.getPublicKey())), a.getPublicKey().length, a.getPublicKey(null, "compressed").length];
        try { a.computeSecret(Buffer.alloc(65)); } catch (e) { out.ecdhBad = e.code; }
        const x1 = crypto.generateKeyPairSync("x25519"), x2 = crypto.generateKeyPairSync("x25519");
        out.x25519 = crypto.diffieHellman({ privateKey: x1.privateKey, publicKey: x2.publicKey }).equals(crypto.diffieHellman({ privateKey: x2.privateKey, publicKey: x1.publicKey }));
        out.x25519Raw = crypto.createPublicKey({ key: x1.publicKey.export({ format: "der", type: "spki" }), format: "der", type: "spki" }).export({ format: "jwk" }).crv;
        const d1 = crypto.getDiffieHellman("modp14"), d2 = crypto.getDiffieHellman("modp14");
        d1.generateKeys(); d2.generateKeys();
        out.dh = [d1.computeSecret(d2.getPublicKey()).equals(d2.computeSecret(d1.getPublicKey())), d1.getPrime().length, d1.getGenerator("hex")];
        const secret = crypto.createSecretKey(Buffer.from("k"));
        out.secret = [secret.type, secret.symmetricKeySize, secret.export().toString()];
        try { secret.export({ format: "pem" }); } catch (e) { out.secretPem = e.code; }
        try { crypto.sign("sha256", Buffer.from("x"), secret); } catch (e) { out.signSecret = [e.code, e.message]; }
        try { crypto.createPrivateKey("garbage"); } catch (e) { out.garbage = e.code; }
        export default out;
    "#)
    .await;
    assert_eq!(
        v,
        json!({
            "ed": [64, true, "ed25519"],
            "pem": [true, true],
            "rsaVerify": true, "pss": true, "oaep": "secret",
            "details": ["public", "rsa", 1024, "65537"],
            "jwk": ["kty", "n", "e", "d", "p", "q", "dp", "dq", "qi"],
            "jwkBack": true,
            "encrypted": [true, true], "noPass": "ERR_OSSL_CRYPTO_INTERRUPTED_OR_CANCELLED",
            "ecdsa": [true, true, 96, "secp384r1"],
            "ecdh": [true, 65, 33], "ecdhBad": "ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY",
            "x25519": true, "x25519Raw": "X25519",
            "dh": [true, 256, "02"],
            "secret": ["secret", 1, "k"], "secretPem": "ERR_INVALID_ARG_VALUE",
            "signSecret": ["ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE", "Invalid key object type secret, expected private."],
            "garbage": "ERR_OSSL_UNSUPPORTED",
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webcrypto_subtle() {
    let v = run(r#"
        const { subtle } = globalThis.crypto;
        const enc = new TextEncoder();
        const out = {};
        const hmac = await subtle.importKey("raw", enc.encode("key"), { name: "HMAC", hash: "SHA-256" }, true, ["sign", "verify"]);
        const mac = await subtle.sign("HMAC", hmac, enc.encode("data"));
        out.hmac = [Buffer.from(mac).toString("hex"), await subtle.verify("HMAC", hmac, mac, enc.encode("data")), hmac.algorithm.length, hmac instanceof CryptoKey];
        const aes = await subtle.generateKey({ name: "AES-GCM", length: 256 }, true, ["encrypt", "decrypt"]);
        const iv = crypto.getRandomValues(new Uint8Array(12));
        const ct = await subtle.encrypt({ name: "AES-GCM", iv }, aes, enc.encode("hello"));
        out.aes = [ct.byteLength, new TextDecoder().decode(await subtle.decrypt({ name: "AES-GCM", iv }, aes, ct))];
        try { await subtle.decrypt({ name: "AES-GCM", iv }, aes, new Uint8Array(ct.byteLength)); } catch (e) { out.aesBad = e.name; }
        const ecdsa = await subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"]);
        const sig = await subtle.sign({ name: "ECDSA", hash: "SHA-256" }, ecdsa.privateKey, enc.encode("m"));
        out.ecdsa = [sig.byteLength, await subtle.verify({ name: "ECDSA", hash: "SHA-256" }, ecdsa.publicKey, sig, enc.encode("m"))];
        const jwk = await subtle.exportKey("jwk", ecdsa.publicKey);
        const back = await subtle.importKey("jwk", jwk, { name: "ECDSA", namedCurve: "P-256" }, true, ["verify"]);
        out.jwk = [jwk.kty, jwk.crv, await subtle.verify({ name: "ECDSA", hash: "SHA-256" }, back, sig, enc.encode("m"))];
        const raw = await subtle.exportKey("raw", ecdsa.publicKey);
        out.raw = raw.byteLength;
        const ed = await subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"]);
        const edSig = await subtle.sign("Ed25519", ed.privateKey, enc.encode("m"));
        out.ed = await subtle.verify("Ed25519", ed.publicKey, edSig, enc.encode("m"));
        const a = await subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, true, ["deriveBits"]);
        const b = await subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, true, ["deriveBits"]);
        const s1 = await subtle.deriveBits({ name: "ECDH", public: b.publicKey }, a.privateKey, 256);
        const s2 = await subtle.deriveBits({ name: "ECDH", public: a.publicKey }, b.privateKey, 256);
        out.ecdh = Buffer.from(s1).equals(Buffer.from(s2));
        const pw = await subtle.importKey("raw", enc.encode("pw"), "PBKDF2", false, ["deriveBits", "deriveKey"]);
        const bits = await subtle.deriveBits({ name: "PBKDF2", salt: enc.encode("salt"), iterations: 1000, hash: "SHA-256" }, pw, 128);
        out.pbkdf2 = Buffer.from(bits).toString("hex");
        const derived = await subtle.deriveKey({ name: "PBKDF2", salt: enc.encode("salt"), iterations: 1000, hash: "SHA-256" }, pw, { name: "AES-GCM", length: 128 }, true, ["encrypt"]);
        out.derived = Buffer.from(await subtle.exportKey("raw", derived)).toString("hex");
        const rsa = await subtle.generateKey({ name: "RSASSA-PKCS1-v1_5", modulusLength: 1024, publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" }, true, ["sign", "verify"]);
        const rsig = await subtle.sign("RSASSA-PKCS1-v1_5", rsa.privateKey, enc.encode("m"));
        out.rsa = [await subtle.verify("RSASSA-PKCS1-v1_5", rsa.publicKey, rsig, enc.encode("m")), rsa.publicKey.algorithm.modulusLength];
        const spki = await subtle.exportKey("spki", rsa.publicKey);
        out.spki = (await subtle.importKey("spki", spki, { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, true, ["verify"])).type;
        const kw = await subtle.generateKey({ name: "AES-KW", length: 128 }, true, ["wrapKey", "unwrapKey"]);
        const wrapped = await subtle.wrapKey("raw", derived, kw, "AES-KW");
        const unwrapped = await subtle.unwrapKey("raw", wrapped, kw, "AES-KW", "AES-GCM", true, ["decrypt"]);
        out.kw = Buffer.from(await subtle.exportKey("raw", unwrapped)).toString("hex") === out.derived;
        out.digest = Buffer.from(await subtle.digest("SHA-256", enc.encode("abc"))).toString("hex").slice(0, 16);
        export default out;
    "#)
    .await;
    assert_eq!(
        v,
        json!({
            "hmac": ["5031fe3d989c6d1537a013fa6e739da23463fdaec3b70137d828e36ace221bd0", true, 24, true],
            "aes": [21, "hello"], "aesBad": "OperationError",
            "ecdsa": [64, true], "jwk": ["EC", "P-256", true], "raw": 65,
            "ed": true, "ecdh": true,
            "pbkdf2": "0a38253555ce37f5c72a6b703f996814",
            "derived": "0a38253555ce37f5c72a6b703f996814",
            "rsa": [true, 1024], "spki": "public", "kw": true,
            "digest": "ba7816bf8f01cfea",
        })
    );
}

/// `WebAssembly`: a module (compiled with wabt from a WAT test file) with imported functions
/// and globals, a defined memory shared with JS, growth (old buffer detached), tables, globals,
/// i64 <-> BigInt, multi-value, traps, JS exceptions through wasm, re-entrancy, link and compile
/// errors. Output compared with Node 22.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webassembly_api() {
    let v = run(r#"
const bytes = Buffer.from("AGFzbQEAAAABJQdgAX8Bf2ADf358AX9gAn5+AX5gAn9/AX9gAAF/YAAAYAACf38CIgMDZW52A2xvZwABA2VudgdyZWVudGVyAAADZW52AWcDfwADDAsAAgADAAQDBQMABgQEAXAAAgUEAQEBCgYGAX8BQQULB3YOBm1lbW9yeQIAB2NvdW50ZXIDAQV0YWJsZQEABmRvdWJsZQACBWFkZDY0AAMHY2FsbExvZwAEA3N1bQAFBGdyb3cABgRidW1wAAcMY2FsbEluZGlyZWN0AAgEdHJhcAAJA2RpdgAKBW91dGVyAAsEcGFpcgAMCQgBAEEACwICAwqPAQsHACAAQQJsCwcAIAAgAXwLGAAgAEKBgICAgICAEEQAAAAAAAD4PxAACykBAn8CQANAIAIgAU8NASADIAAgAmotAABqIQMgAkEBaiECDAALCyADCwYAIABAAAsLACMBIwBqJAEjAQsJACABIAARAAALAwAACwcAIAAgAW0LCgAgABABQegHagsGAEEBQQILCxABAEEQCwpoZWxsbyB3YXNt", "base64");
const out = {};
out.valid = [WebAssembly.validate(bytes), WebAssembly.validate(new Uint8Array([0, 97, 115, 109, 2, 0, 0, 0]))];
const module = await WebAssembly.compile(bytes);
out.imports = WebAssembly.Module.imports(module);
out.exports = WebAssembly.Module.exports(module).map((e) => e.name + ":" + e.kind);
const logged = [];
let instance;
const g = 7;
({ instance } = await WebAssembly.instantiate(bytes, { env: {
  log: (a, b, c) => { logged.push([a, typeof b, String(b), c]); return a + 1; },
  reenter: (x) => instance.exports.double(x),
  g,
} }));
const e = instance.exports;
out.double = e.double(21);
out.add64 = String(e.add64(2n ** 62n, 5n));
out.callLog = e.callLog(41);
out.logged = logged;
const mem = e.memory;
out.hello = new TextDecoder().decode(new Uint8Array(mem.buffer, 16, 10));
new Uint8Array(mem.buffer).set([1, 2, 3, 250], 100);
out.sum = e.sum(100, 4);
const before = mem.buffer;
out.grow = [e.grow(2), mem.buffer.byteLength, before.byteLength, mem.buffer === mem.buffer, e.memory === mem];
out.growJs = [mem.grow(1), mem.buffer.byteLength];
out.growFail = e.grow(100);
out.bump = [e.bump(), e.bump(), e.counter.value];
e.counter.value = 1;
out.counter = e.counter.value;
out.table = [e.table.length, e.table.get(0) === e.double, e.callIndirect(0, 5)];
const err = (f) => { try { f(); return "no error"; } catch (x) { return [x.constructor.name, x instanceof WebAssembly.RuntimeError, x.message]; } };
out.trap = err(() => e.trap());
out.div0 = err(() => e.div(1, 0));
out.indirectOOB = err(() => e.callIndirect(5, 1));
out.outer = e.outer(4);
out.pair = e.pair();
const thrower = new WebAssembly.Instance(module, { env: { log: () => { throw new Error("from js"); }, reenter: () => 0, g: 1 } });
out.jsThrow = err(() => thrower.exports.callLog(1));
out.link = err(() => new WebAssembly.Instance(module, { env: { log: 5, reenter: () => 0, g: 1 } }))[0];
out.compileErr = err(() => new WebAssembly.Module(new Uint8Array([1, 2, 3])))[0];
const m2 = new WebAssembly.Memory({ initial: 1, maximum: 3 });
new Uint8Array(m2.buffer)[0] = 42;
out.memory = [m2.buffer.byteLength, m2.grow(2), m2.buffer.byteLength, new Uint8Array(m2.buffer)[0]];
const gl = new WebAssembly.Global({ value: "i64", mutable: true }, 5n);
gl.value = 6n;
out.global = String(gl.value);
out.tag = Object.prototype.toString.call(WebAssembly);
export default out;
"#)
    .await;
    let expected: Value = serde_json::from_str(r#"{"valid":[true,false],"imports":[{"module":"env","name":"log","kind":"function"},{"module":"env","name":"reenter","kind":"function"},{"module":"env","name":"g","kind":"global"}],"exports":["memory:memory","counter:global","table:table","double:function","add64:function","callLog:function","sum:function","grow:function","bump:function","callIndirect:function","trap:function","div:function","outer:function","pair:function"],"double":42,"add64":"4611686018427387909","callLog":42,"logged":[[41,"bigint","9007199254740993",1.5]],"hello":"hello wasm","sum":256,"grow":[1,196608,0,true,true],"growJs":[3,262144],"growFail":-1,"bump":[12,19,19],"counter":1,"table":[2,true,10],"trap":["RuntimeError",true,"unreachable"],"div0":["RuntimeError",true,"divide by zero"],"indirectOOB":["RuntimeError",true,"table index is out of bounds"],"outer":1008,"pair":[1,2],"jsThrow":["Error",false,"from js"],"link":"LinkError","compileErr":"CompileError","memory":[65536,1,196608,42],"global":"6","tag":"[object WebAssembly]"}"#).unwrap();
    assert_eq!(v, expected);
}

/// `node:tls` client/server and `node:https` with a test CA (certificates in the source):
/// ALPN, verification against `ca`, Node's verification error codes (unknown issuer, wrong
/// host, self-signed), `rejectUnauthorized: false`, https.createServer, https.request with and
/// without an Agent. Output compared with Node 22.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_and_https() {
    let v = run(r#"
import tls from "node:tls";
import https from "node:https";
const certs = {"ca": "-----BEGIN CERTIFICATE-----\nMIIBijCCATGgAwIBAgIUVs+EfMS/+Ro7R0lcA4uc/AhPkfcwCgYIKoZIzj0EAwIw\nGjEYMBYGA1UEAwwPcGktanNydCB0ZXN0IENBMCAXDTI2MTAyMjE0NTQ1NloYDzIx\nMjYwOTI4MTQ1NDU2WjAaMRgwFgYDVQQDDA9waS1qc3J0IHRlc3QgQ0EwWTATBgcq\nhkjOPQIBBggqhkjOPQMBBwNCAAQXCcWuLg51sJCkC2Jjdx3JnX9g30cJm2/Q+ntm\nIKTAK1Qf33OATy1sjP94DopCai3wP+eOi5/On5xoGRfrwSuLo1MwUTAdBgNVHQ4E\nFgQUT/pxUc0B07VmR1bUJ1CjH3qtNwYwHwYDVR0jBBgwFoAUT/pxUc0B07VmR1bU\nJ1CjH3qtNwYwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNHADBEAiBAJYPI\nkFx24T48mhhbpia1ZkznObsjhiqwHClxPMIeDgIgJThIRiRm0g2PuaoNQFBJzvKx\nnT4Y2EGoPBoqvDax6+k=\n-----END CERTIFICATE-----\n", "leafKey": "-----BEGIN EC PRIVATE KEY-----\nMHcCAQEEIDMUcB3eZ4nh1AzrsEMvsmqcOxfnBpjfLWJUJWivV79QoAoGCCqGSM49\nAwEHoUQDQgAECsujzoaPh8daHDbzr+W8cUzLjRqqoXS57N2//PYAZ7IMr47Pkxto\nbCvCjp2YpgHhoxfaNUQSnnpnt88TTmKQew==\n-----END EC PRIVATE KEY-----\n", "leafCert": "-----BEGIN CERTIFICATE-----\nMIIByjCCAW+gAwIBAgIUY2YuzVK4HDbIzlyNdAw/u74U/7UwCgYIKoZIzj0EAwIw\nGjEYMBYGA1UEAwwPcGktanNydCB0ZXN0IENBMCAXDTI2MTAyMjE0NTQ1NloYDzIx\nMjYwOTI4MTQ1NDU2WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIB\nBggqhkjOPQMBBwNCAAQKy6POho+Hx1ocNvOv5bxxTMuNGqqhdLns3b/89gBnsgyv\njs+TG2hsK8KOnZimAeGjF9o1RBKeeme3zxNOYpB7o4GWMIGTMBoGA1UdEQQTMBGC\nCWxvY2FsaG9zdIcEfwAAATAJBgNVHRMEAjAAMAsGA1UdDwQEAwIHgDAdBgNVHSUE\nFjAUBggrBgEFBQcDAQYIKwYBBQUHAwIwHQYDVR0OBBYEFEnI8FK6Ovo9sspTt0r9\nUzpPhZDQMB8GA1UdIwQYMBaAFE/6cVHNAdO1ZkdW1CdQox96rTcGMAoGCCqGSM49\nBAMCA0kAMEYCIQCSxhvJ5bcdbziBydqRjR7cFGlcu7mBHseV5NDOkRkd7QIhAK0K\nHb6U4U6rbzNvIl+I/L+wKz97yKaglZ5BgB0aHSxB\n-----END CERTIFICATE-----\n", "selfKey": "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgdgY8dymE4ogssKPY\nwiPUuUK/Vd7p24Pf66MkNRxPvkWhRANCAARyoIUirzoFMXRzf/h3glvrCpsBC834\n5x9X36pyLsrgn3025Y7liAVHHIsIW2I2G0esamjWwQLxTITOXoFe1pUW\n-----END PRIVATE KEY-----\n", "selfCert": "-----BEGIN CERTIFICATE-----\nMIIBmzCCAUGgAwIBAgIUS/1oasIwSWyNdibO8FDTpMXj8kkwCgYIKoZIzj0EAwIw\nFDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTAyMjE0NTQ1NloYDzIxMjYwOTI4\nMTQ1NDU2WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO\nPQMBBwNCAARyoIUirzoFMXRzf/h3glvrCpsBC8345x9X36pyLsrgn3025Y7liAVH\nHIsIW2I2G0esamjWwQLxTITOXoFe1pUWo28wbTAdBgNVHQ4EFgQUJ1PVpBqrN9iN\n3/LdV3jFPioUuDwwHwYDVR0jBBgwFoAUJ1PVpBqrN9iN3/LdV3jFPioUuDwwDwYD\nVR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCgYIKoZI\nzj0EAwIDSAAwRQIhAK2VRsHWv2v8SxSKEM/mi+KImGGhovgXStaw3D1xRujIAiBq\nJc1DQbnRlymy8y/B1HyIz5UYShVBzBNH2IX+Y4caOA==\n-----END CERTIFICATE-----\n"};
const out = {};
const server = tls.createServer({ key: certs.leafKey, cert: certs.leafCert, ALPNProtocols: ["h2", "echo/1"] }, (socket) => {
  socket.on("data", (d) => socket.write(`echo:${d}:${socket.alpnProtocol}`));
  socket.on("end", () => socket.end());
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const talk = (opts) => new Promise((resolve) => {
  const s = tls.connect({ port, host: "127.0.0.1", ...opts }, () => {
    const info = { authorized: s.authorized, authorizationError: s.authorizationError, protocol: s.getProtocol(), alpn: s.alpnProtocol, cn: s.getPeerCertificate().subject?.CN, issuer: s.getPeerCertificate(true).issuer?.CN };
    s.write("ping");
    s.once("data", (d) => { info.reply = String(d); s.end(); resolve(info); });
  });
  s.on("error", (e) => resolve({ error: e.code, message: e.message }));
});
out.trusted = await talk({ ca: certs.ca, servername: "localhost", ALPNProtocols: ["echo/1"] });
out.untrusted = await talk({ servername: "localhost" });
out.insecure = await talk({ servername: "localhost", rejectUnauthorized: false });
out.wrongName = await talk({ ca: certs.ca, servername: "example.com" });
const self = tls.createServer({ key: certs.selfKey, cert: certs.selfCert }, (s) => s.end("bye"));
await new Promise((r) => self.listen(0, "127.0.0.1", r));
out.selfSigned = await new Promise((resolve) => {
  const s = tls.connect({ port: self.address().port, host: "localhost" });
  s.on("secureConnect", () => resolve("connected"));
  s.on("error", (e) => resolve([e.code, e.message]));
});
self.close();
const web = https.createServer({ key: certs.leafKey, cert: certs.leafCert }, (req, res) => {
  res.setHeader("content-type", "text/plain");
  res.end(`hello ${req.method} ${req.url} over ${req.socket.getProtocol()}`);
});
await new Promise((r) => web.listen(0, "127.0.0.1", r));
const get = (opts) => new Promise((resolve) => {
  const req = https.request({ host: "localhost", port: web.address().port, path: "/x?y=1", ...opts }, (res) => {
    let body = "";
    res.on("data", (d) => (body += d));
    res.on("end", () => resolve({ status: res.statusCode, body }));
  });
  req.on("error", (e) => resolve({ error: e.code }));
  req.end();
});
out.https = await get({ ca: certs.ca });
out.httpsAgent = await get({ agent: new https.Agent({ ca: certs.ca, keepAlive: true }), method: "POST" });
out.httpsReject = await get({});
web.close();
server.close();
export default out;
"#)
    .await;
    let expected: Value = serde_json::from_str(r#"{"trusted":{"authorized":true,"authorizationError":null,"protocol":"TLSv1.3","alpn":"echo/1","cn":"localhost","issuer":"pi-jsrt test CA","reply":"echo:ping:echo/1"},"untrusted":{"error":"UNABLE_TO_VERIFY_LEAF_SIGNATURE","message":"unable to verify the first certificate"},"insecure":{"authorized":false,"authorizationError":"UNABLE_TO_VERIFY_LEAF_SIGNATURE","protocol":"TLSv1.3","alpn":false,"cn":"localhost","issuer":"pi-jsrt test CA","reply":"echo:ping:false"},"wrongName":{"error":"ERR_TLS_CERT_ALTNAME_INVALID","message":"Hostname/IP does not match certificate's altnames: Host: example.com. is not in the cert's altnames: DNS:localhost, IP Address:127.0.0.1"},"selfSigned":["DEPTH_ZERO_SELF_SIGNED_CERT","self-signed certificate"],"https":{"status":200,"body":"hello GET /x?y=1 over TLSv1.3"},"httpsAgent":{"status":200,"body":"hello POST /x?y=1 over TLSv1.3"},"httpsReject":{"error":"UNABLE_TO_VERIFY_LEAF_SIGNATURE"}}"#).unwrap();
    assert_eq!(v, expected);
}
