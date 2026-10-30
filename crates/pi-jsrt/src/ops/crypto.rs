//! Hashing, HMAC, randomness and key derivation ops (`crypto.createHash`, `randomUUID`,
//! `getRandomValues`, `pbkdf2`, `scrypt`, `hkdf`).

use crate::js::{JsArrayBuffer, JsTypedArray};
use crate::js::{Context, JsArgs, JsResult, JsValue};
use rand::RngCore;
use sha2::Digest;

use super::{bytes_arg, bytes_to_js};
use crate::engine::{OpFn, arg_f64, arg_string, js_str, type_error};

/// Canonical digest name for a Node/OpenSSL digest spelling (`SHA256`, `RSA-SHA256`,
/// `sha256WithRSAEncryption`, `SHA-256`, `sha512-256`, `RSA-SHA3-256`, `rmd160`, ...).
pub(crate) fn digest_name(alg: &str) -> Option<&'static str> {
    let mut a = alg.to_ascii_lowercase();
    for prefix in ["rsa-", "ecdsa-with-", "id-rsassa-pkcs1-v1_5-with-", "ssl3-"] {
        if let Some(rest) = a.strip_prefix(prefix) {
            a = rest.to_string();
        }
    }
    for suffix in ["withrsaencryption", "withrsa"] {
        if let Some(rest) = a.strip_suffix(suffix) {
            a = rest.to_string();
        }
    }
    Some(match a.replace(['-', '/', '_'], "").as_str() {
        "md5" => "md5",
        "sha1" | "sha12" => "sha1",
        "sha224" => "sha224",
        "sha256" => "sha256",
        "sha384" => "sha384",
        "sha512" => "sha512",
        "sha512224" => "sha512-224",
        "sha512256" => "sha512-256",
        "sha3224" => "sha3-224",
        "sha3256" => "sha3-256",
        "sha3384" => "sha3-384",
        "sha3512" => "sha3-512",
        "shake128" => "shake128",
        "shake256" => "shake256",
        "ripemd" | "ripemd160" | "rmd160" => "ripemd160",
        "blake2b512" => "blake2b512",
        "blake2s256" => "blake2s256",
        "sm3" => "sm3",
        "md5sha1" => "md5-sha1",
        _ => return None,
    })
}

/// Digests OpenSSL 3 only has in its legacy provider (not loaded by Node).
fn legacy_digest(alg: &str) -> bool {
    matches!(alg.to_ascii_lowercase().as_str(), "md4" | "rsa-md4" | "md4withrsaencryption" | "mdc2" | "rsa-mdc2" | "whirlpool")
}

/// A digest failure with Node's message and code.
#[derive(Debug, Clone)]
pub(crate) struct DigestError(pub &'static str, pub Option<&'static str>);

fn digest_unsupported(alg: &str) -> DigestError {
    if legacy_digest(alg) {
        DigestError("error:0308010C:digital envelope routines::unsupported", Some("ERR_OSSL_EVP_UNSUPPORTED"))
    } else {
        DigestError("Digest method not supported", None)
    }
}

/// An incremental hash (`crypto.createHash`) for any supported digest, XOFs included.
pub(crate) enum Hasher {
    /// A fixed-size digest and its block size (for HMAC).
    Dyn(Box<dyn digest::DynDigest>, usize),
    Shake128(sha3::Shake128, usize),
    Shake256(sha3::Shake256, usize),
    Md5Sha1(md5::Md5, sha1::Sha1),
}

impl Hasher {
    /// `out_len`: Node's `outputLength` (XOFs: any length; others: must be the digest size).
    pub(crate) fn new(alg: &str, out_len: Option<usize>) -> Result<Self, DigestError> {
        let name = digest_name(alg).ok_or_else(|| digest_unsupported(alg))?;
        let h = match name {
            "md5" => Self::dynd::<md5::Md5>(),
            "sha1" => Self::dynd::<sha1::Sha1>(),
            "sha224" => Self::dynd::<sha2::Sha224>(),
            "sha256" => Self::dynd::<sha2::Sha256>(),
            "sha384" => Self::dynd::<sha2::Sha384>(),
            "sha512" => Self::dynd::<sha2::Sha512>(),
            "sha512-224" => Self::dynd::<sha2::Sha512_224>(),
            "sha512-256" => Self::dynd::<sha2::Sha512_256>(),
            "sha3-224" => Self::dynd::<sha3::Sha3_224>(),
            "sha3-256" => Self::dynd::<sha3::Sha3_256>(),
            "sha3-384" => Self::dynd::<sha3::Sha3_384>(),
            "sha3-512" => Self::dynd::<sha3::Sha3_512>(),
            "ripemd160" => Self::dynd::<ripemd::Ripemd160>(),
            "blake2b512" => Self::dynd::<blake2::Blake2b512>(),
            "blake2s256" => Self::dynd::<blake2::Blake2s256>(),
            "sm3" => Self::dynd::<sm3::Sm3>(),
            "md5-sha1" => Self::Md5Sha1(Default::default(), Default::default()),
            "shake128" => Self::Shake128(Default::default(), out_len.unwrap_or(16)),
            _ => Self::Shake256(Default::default(), out_len.unwrap_or(32)),
        };
        if let Some(n) = out_len
            && !matches!(h, Self::Shake128(..) | Self::Shake256(..))
            && n != h.output_len()
        {
            return Err(DigestError("error:030000B2:digital envelope routines::not XOF or invalid length", Some("ERR_OSSL_EVP_NOT_XOF_OR_INVALID_LENGTH")));
        }
        Ok(h)
    }

    fn dynd<D: digest::DynDigest + digest::core_api::BlockSizeUser + Default + 'static>() -> Self {
        Self::Dyn(Box::new(D::default()), D::block_size())
    }

    pub(crate) fn output_len(&self) -> usize {
        match self {
            Self::Dyn(d, _) => d.output_size(),
            Self::Shake128(_, n) | Self::Shake256(_, n) => *n,
            Self::Md5Sha1(..) => 36,
        }
    }

    fn is_xof(&self) -> bool {
        matches!(self, Self::Shake128(..) | Self::Shake256(..))
    }

    /// The copy of an XOF may change its output length (`hash.copy({ outputLength })`).
    pub(crate) fn with_output_len(mut self, n: usize) -> Self {
        if let Self::Shake128(_, len) | Self::Shake256(_, len) = &mut self {
            *len = n;
        }
        self
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        use sha3::digest::Update;
        match self {
            Self::Dyn(d, _) => d.update(data),
            Self::Shake128(h, _) => h.update(data),
            Self::Shake256(h, _) => h.update(data),
            Self::Md5Sha1(a, b) => {
                Digest::update(a, data);
                Digest::update(b, data);
            }
        }
    }

    pub(crate) fn finalize(self) -> Vec<u8> {
        use sha3::digest::{ExtendableOutput, XofReader};
        match self {
            Self::Dyn(d, _) => d.finalize().to_vec(),
            Self::Shake128(h, n) => {
                let mut out = vec![0u8; n];
                h.finalize_xof().read(&mut out);
                out
            }
            Self::Shake256(h, n) => {
                let mut out = vec![0u8; n];
                h.finalize_xof().read(&mut out);
                out
            }
            Self::Md5Sha1(a, b) => {
                let mut out = a.finalize().to_vec();
                out.extend_from_slice(&b.finalize());
                out
            }
        }
    }

    /// HMAC block size (`None` for XOFs, which OpenSSL's HMAC rejects).
    fn block_size(&self) -> Option<usize> {
        Some(match self {
            Self::Dyn(_, bs) => *bs,
            Self::Md5Sha1(..) => 64,
            _ => return None,
        })
    }
}

impl Clone for Hasher {
    fn clone(&self) -> Self {
        match self {
            Self::Dyn(d, bs) => Self::Dyn(d.box_clone(), *bs),
            Self::Shake128(h, n) => Self::Shake128(h.clone(), *n),
            Self::Shake256(h, n) => Self::Shake256(h.clone(), *n),
            Self::Md5Sha1(a, b) => Self::Md5Sha1(a.clone(), b.clone()),
        }
    }
}

/// An incremental HMAC over any non-XOF digest.
#[derive(Clone)]
pub(crate) struct HmacState {
    inner: Hasher,
    outer: Hasher,
}

impl HmacState {
    pub(crate) fn new(alg: &str, key: &[u8]) -> Result<Self, DigestError> {
        let base = Hasher::new(alg, None)?;
        let bs = base.block_size().ok_or(DigestError("error:00000000:lib(0)::reason(0)", None))?;
        let mut k = if key.len() > bs {
            let mut h = base.clone();
            h.update(key);
            h.finalize()
        } else {
            key.to_vec()
        };
        k.resize(bs, 0);
        let mut inner = base.clone();
        inner.update(&k.iter().map(|b| b ^ 0x36).collect::<Vec<_>>());
        let mut outer = base;
        outer.update(&k.iter().map(|b| b ^ 0x5c).collect::<Vec<_>>());
        Ok(Self { inner, outer })
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    pub(crate) fn finalize(self) -> Vec<u8> {
        let mut outer = self.outer;
        outer.update(&self.inner.finalize());
        outer.finalize()
    }

    fn output_len(&self) -> usize {
        self.outer.output_len()
    }
}

/// Hash `data` with a Node algorithm name. Returns `None` for unknown algorithms.
pub(crate) fn hash(alg: &str, data: &[u8]) -> Option<Vec<u8>> {
    let mut h = Hasher::new(alg, None).ok()?;
    h.update(data);
    Some(h.finalize())
}

/// Run `$body` with `$D` bound to the digest type named by the canonical `$name`.
macro_rules! with_digest {
    ($name:expr, $D:ident => $body:expr) => {
        match $name {
            "md5" => {
                type $D = md5::Md5;
                Some($body)
            }
            "sha1" => {
                type $D = sha1::Sha1;
                Some($body)
            }
            "sha224" => {
                type $D = sha2::Sha224;
                Some($body)
            }
            "sha256" => {
                type $D = sha2::Sha256;
                Some($body)
            }
            "sha384" => {
                type $D = sha2::Sha384;
                Some($body)
            }
            "sha512" => {
                type $D = sha2::Sha512;
                Some($body)
            }
            "sha512-224" => {
                type $D = sha2::Sha512_224;
                Some($body)
            }
            "sha512-256" => {
                type $D = sha2::Sha512_256;
                Some($body)
            }
            _ => None,
        }
    };
}
#[allow(unused_imports)]
pub(crate) use with_digest;

fn digest_error(ctx: &mut Context, e: DigestError) -> crate::js::JsError {
    let props: Vec<(&str, JsValue)> = e.1.map(|c| vec![("code", js_str(c))]).unwrap_or_default();
    super::coded_error(ctx, "Error", e.0, &props)
}

fn op_hash(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let alg = arg_string(args, 0, ctx)?;
    let data = bytes_arg(args.get_or_undefined(1), ctx)?;
    match Hasher::new(&alg, None) {
        Ok(mut h) => {
            h.update(&data);
            bytes_to_js(&h.finalize(), ctx)
        }
        Err(e) => Err(digest_error(ctx, e)),
    }
}

fn op_hmac(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let alg = arg_string(args, 0, ctx)?;
    let key = bytes_arg(args.get_or_undefined(1), ctx)?;
    let data = bytes_arg(args.get_or_undefined(2), ctx)?;
    match HmacState::new(&alg, &key) {
        Ok(mut h) => {
            h.update(&data);
            bytes_to_js(&h.finalize(), ctx)
        }
        Err(e) => Err(digest_error(ctx, e)),
    }
}

fn opt_len(v: &JsValue, ctx: &mut Context) -> JsResult<Option<usize>> {
    if v.is_null_or_undefined() { Ok(None) } else { Ok(Some(v.to_number(ctx)? as usize)) }
}

/// `ops.hashNew(alg, outputLength?) -> handle`
fn op_hash_new(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let alg = arg_string(args, 0, ctx)?;
    let out_len = opt_len(args.get_or_undefined(1), ctx)?;
    match Hasher::new(&alg, out_len) {
        Ok(h) => Ok(super::native_object(h)),
        Err(e) => Err(digest_error(ctx, e)),
    }
}

/// `ops.hashUpdate(handle, bytes)`
fn op_hash_update(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let data = bytes_arg(args.get_or_undefined(1), ctx)?;
    let v = args.get_or_undefined(0);
    if super::with_native::<HmacState, _>(v, |h| h.update(&data)).is_err() {
        super::with_native::<Hasher, _>(v, |h| h.update(&data))?;
    }
    Ok(JsValue::undefined())
}

/// `ops.hashDigest(handle) -> bytes` (the handle stays usable for nothing else).
fn op_hash_digest(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = args.get_or_undefined(0);
    let out = match super::with_native::<HmacState, _>(v, |h| h.clone().finalize()) {
        Ok(out) => out,
        Err(_) => super::with_native::<Hasher, _>(v, |h| h.clone().finalize())?,
    };
    bytes_to_js(&out, ctx)
}

/// `ops.hashCopy(handle, outputLength?) -> handle`
fn op_hash_copy(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let out_len = opt_len(args.get_or_undefined(1), ctx)?;
    let h = super::with_native::<Hasher, _>(args.get_or_undefined(0), |h| h.clone())?;
    if let Some(n) = out_len {
        if !h.is_xof() && n != h.output_len() {
            return Err(digest_error(ctx, DigestError("error:030000B2:digital envelope routines::not XOF or invalid length", Some("ERR_OSSL_EVP_NOT_XOF_OR_INVALID_LENGTH"))));
        }
        return Ok(super::native_object(h.with_output_len(n)));
    }
    Ok(super::native_object(h))
}

/// `ops.hmacNew(alg, key) -> handle` (updated/digested with hashUpdate/hashDigest)
fn op_hmac_new(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let alg = arg_string(args, 0, ctx)?;
    let key = bytes_arg(args.get_or_undefined(1), ctx)?;
    match HmacState::new(&alg, &key) {
        Ok(h) => Ok(super::native_object(h)),
        Err(e) => Err(digest_error(ctx, e)),
    }
}

fn op_random_uuid(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(js_str(&uuid::Uuid::new_v4().to_string()))
}

fn op_random_bytes(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let n = arg_f64(args, 0, ctx)?;
    if !(0.0..=(1u64 << 31) as f64).contains(&n) {
        return Err(crate::js::JsNativeError::range().with_message("The value of \"size\" is out of range.").into());
    }
    let mut buf = vec![0u8; n as usize];
    rand::thread_rng().fill_bytes(&mut buf);
    bytes_to_js(&buf, ctx)
}

fn op_fill_random(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = args.get_or_undefined(0).clone();
    let obj = v.as_object().ok_or_else(|| type_error("The data argument must be an integer-type TypedArray"))?;
    let ta = JsTypedArray::from_object(obj).map_err(|_| type_error("The data argument must be an integer-type TypedArray"))?;
    let offset = ta.byte_offset(ctx)?;
    let len = ta.byte_length(ctx)?;
    if len > 65536 {
        return Err(crate::js::JsNativeError::error()
            .with_message(format!(
                "The ArrayBufferView's byte length ({len}) exceeds the number of bytes of entropy available via this API (65536)"
            ))
            .into());
    }
    let buffer = ta.buffer(ctx)?;
    if let Some(b) = buffer.as_object()
        && let Ok(ab) = JsArrayBuffer::from_object(b)
        && let Some(data) = ab.data_mut()
    {
        rand::thread_rng().fill_bytes(&mut data[offset..offset + len]);
    }
    Ok(v)
}

pub(crate) const WEB_OPS: &[(&str, usize, OpFn)] = &[
    ("hash", 2, op_hash),
    ("hmac", 3, op_hmac),
    ("hashNew", 2, op_hash_new),
    ("hashUpdate", 2, op_hash_update),
    ("hashDigest", 1, op_hash_digest),
    ("hashCopy", 2, op_hash_copy),
    ("hmacNew", 2, op_hmac_new),
    ("randomUUID", 0, op_random_uuid),
    ("fillRandom", 1, op_fill_random),
    ("randomBytes", 1, op_random_bytes),
];

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[];

// ---------------------------------------------------------------------------
// Key derivation (pbkdf2, scrypt, hkdf)
// ---------------------------------------------------------------------------

use super::cipher::CipherError;

fn invalid_digest(alg: &str) -> CipherError {
    CipherError::new("TypeError", &format!("Invalid digest: {alg}"), Some("ERR_CRYPTO_INVALID_DIGEST"))
}

pub(crate) fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32, keylen: usize, digest: &str) -> Result<Vec<u8>, CipherError> {
    let name = digest_name(digest).ok_or_else(|| invalid_digest(digest))?;
    let mut out = vec![0u8; keylen];
    if with_digest!(name, D => pbkdf2::pbkdf2_hmac::<D>(password, salt, iterations, &mut out)).is_some() {
        return Ok(out);
    }
    // Other digests (SHA-3, BLAKE2, RIPEMD-160, SM3, ...): PBKDF2 over the generic HMAC.
    let keyed = HmacState::new(name, password).map_err(|_| CipherError::new("Error", "Deriving bits failed", None))?;
    let hlen = keyed.output_len();
    for (i, block) in out.chunks_mut(hlen).enumerate() {
        let mut h = keyed.clone();
        h.update(salt);
        h.update(&(i as u32 + 1).to_be_bytes());
        let mut u = h.finalize();
        let mut t = u.clone();
        for _ in 1..iterations {
            let mut h = keyed.clone();
            h.update(&u);
            u = h.finalize();
            t.iter_mut().zip(&u).for_each(|(a, b)| *a ^= b);
        }
        block.copy_from_slice(&t[..block.len()]);
    }
    Ok(out)
}

pub(crate) fn scrypt(password: &[u8], salt: &[u8], keylen: usize, n: u64, r: u32, p: u32, maxmem: u64) -> Result<Vec<u8>, CipherError> {
    let invalid = |why: &str| CipherError::new("RangeError", &format!("Invalid scrypt params: {why}"), Some("ERR_CRYPTO_INVALID_SCRYPT_PARAMS"));
    if n < 2 || !n.is_power_of_two() {
        return Err(invalid("error:030000AC:digital envelope routines::invalid N"));
    }
    if 128 * n * r as u64 > maxmem {
        return Err(invalid("error:030000AC:digital envelope routines::memory limit exceeded"));
    }
    let params = scrypt::Params::new(n.trailing_zeros() as u8, r, p, keylen.max(10)).map_err(|e| invalid(&e.to_string()))?;
    let mut out = vec![0u8; keylen];
    scrypt::scrypt(password, salt, &params, &mut out).map_err(|e| invalid(&e.to_string()))?;
    Ok(out)
}

pub(crate) fn hkdf(digest: &str, ikm: &[u8], salt: &[u8], info: &[u8], keylen: usize) -> Result<Vec<u8>, CipherError> {
    let name = digest_name(digest).ok_or_else(|| invalid_digest(digest))?;
    let mut out = vec![0u8; keylen];
    let ok = match with_digest!(name, D => hkdf::Hkdf::<D>::new(Some(salt), ikm).expand(info, &mut out).is_ok()) {
        Some(ok) => ok,
        None => hkdf_generic(name, ikm, salt, info, &mut out)?,
    };
    if !ok {
        return Err(CipherError::new("RangeError", "Invalid key length", Some("ERR_CRYPTO_INVALID_KEYLEN")));
    }
    Ok(out)
}

/// HKDF (RFC 5869) over the generic HMAC, for digests without a typed fast path.
fn hkdf_generic(name: &str, ikm: &[u8], salt: &[u8], info: &[u8], out: &mut [u8]) -> Result<bool, CipherError> {
    let failed = || CipherError::new("Error", "Deriving bits failed", None);
    let probe = HmacState::new(name, b"").map_err(|_| failed())?;
    let hlen = probe.output_len();
    if out.len() > 255 * hlen {
        return Ok(false);
    }
    let salt = if salt.is_empty() { vec![0u8; hlen] } else { salt.to_vec() };
    let mut extract = HmacState::new(name, &salt).map_err(|_| failed())?;
    extract.update(ikm);
    let prk = extract.finalize();
    let keyed = HmacState::new(name, &prk).map_err(|_| failed())?;
    let mut t = Vec::new();
    for (i, block) in out.chunks_mut(hlen).enumerate() {
        let mut h = keyed.clone();
        h.update(&t);
        h.update(info);
        h.update(&[i as u8 + 1]);
        t = h.finalize();
        block.copy_from_slice(&t[..block.len()]);
    }
    Ok(true)
}

pub(crate) fn to_error(e: CipherError) -> crate::Error {
    crate::Error::Js(crate::error::JsException { name: e.kind.into(), message: e.message, stack: None, code: e.code.map(Into::into) })
}

/// Run a crypto job: synchronously, or on tokio's blocking pool when `asynchronous` (returns a
/// promise of the bytes).
pub(crate) fn job(ctx: &mut Context, asynchronous: bool, f: impl FnOnce() -> Result<Vec<u8>, CipherError> + Send + 'static) -> JsResult<JsValue> {
    if !asynchronous {
        return match f() {
            Ok(b) => bytes_to_js(&b, ctx),
            Err(e) => Err(e.throw(ctx)),
        };
    }
    Ok(crate::engine::spawn_result(
        ctx,
        async move {
            match tokio::task::spawn_blocking(f).await {
                Ok(r) => r.map_err(to_error),
                Err(e) => Err(crate::Error::Host(e.to_string())),
            }
        },
        |b, ctx| bytes_to_js(&b, ctx),
    ))
}

/// `ops.pbkdf2(password, salt, iterations, keylen, digest, async)`
fn op_pbkdf2(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let password = bytes_arg(args.get_or_undefined(0), ctx)?;
    let salt = bytes_arg(args.get_or_undefined(1), ctx)?;
    let iterations = arg_f64(args, 2, ctx)? as u32;
    let keylen = arg_f64(args, 3, ctx)? as usize;
    let digest = arg_string(args, 4, ctx)?;
    let asynchronous = args.get_or_undefined(5).to_boolean();
    job(ctx, asynchronous, move || pbkdf2(&password, &salt, iterations, keylen, &digest))
}

/// `ops.scrypt(password, salt, keylen, N, r, p, maxmem, async)`
fn op_scrypt(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let password = bytes_arg(args.get_or_undefined(0), ctx)?;
    let salt = bytes_arg(args.get_or_undefined(1), ctx)?;
    let keylen = arg_f64(args, 2, ctx)? as usize;
    let n = arg_f64(args, 3, ctx)? as u64;
    let r = arg_f64(args, 4, ctx)? as u32;
    let p = arg_f64(args, 5, ctx)? as u32;
    let maxmem = arg_f64(args, 6, ctx)? as u64;
    let asynchronous = args.get_or_undefined(7).to_boolean();
    job(ctx, asynchronous, move || scrypt(&password, &salt, keylen, n, r, p, maxmem))
}

/// `ops.hkdf(digest, ikm, salt, info, keylen, async)`
fn op_hkdf(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let digest = arg_string(args, 0, ctx)?;
    let ikm = bytes_arg(args.get_or_undefined(1), ctx)?;
    let salt = bytes_arg(args.get_or_undefined(2), ctx)?;
    let info = bytes_arg(args.get_or_undefined(3), ctx)?;
    let keylen = arg_f64(args, 4, ctx)? as usize;
    let asynchronous = args.get_or_undefined(5).to_boolean();
    job(ctx, asynchronous, move || hkdf(&digest, &ikm, &salt, &info, keylen))
}

/// `ops.digestInfo(name) -> canonical name | undefined`
fn op_digest_name(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_string(args, 0, ctx)?;
    Ok(digest_name(&name).map(js_str).unwrap_or_default())
}

pub(crate) const KDF_OPS: &[(&str, usize, OpFn)] = &[
    ("pbkdf2", 6, op_pbkdf2),
    ("scrypt", 8, op_scrypt),
    ("hkdf", 6, op_hkdf),
    ("digestName", 1, op_digest_name),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn kdfs_match_node() {
        assert_eq!(hex(&pbkdf2(b"pw", b"salt", 1000, 16, "sha256").unwrap()), "0a38253555ce37f5c72a6b703f996814");
        assert_eq!(hex(&scrypt(b"pw", b"salt", 16, 16384, 8, 1, 32 << 20).unwrap()), "c0b515908e61334cae6d6003c00be60e");
        assert_eq!(hex(&hkdf("sha256", b"key", b"salt", b"info", 16).unwrap()), "9ca0d662557439e3b83365f2da4626d3");
        assert_eq!(pbkdf2(b"a", b"b", 1, 32, "foo").unwrap_err().message, "Invalid digest: foo");
        assert!(scrypt(b"a", b"b", 64, 1 << 20, 8, 1, 32 << 20).unwrap_err().message.contains("memory limit exceeded"));
        assert_eq!(digest_name("RSA-SHA256"), Some("sha256"));
        assert_eq!(digest_name("sha256WithRSAEncryption"), Some("sha256"));
        assert_eq!(digest_name("SHA-512/256"), Some("sha512-256"));
    }
}
