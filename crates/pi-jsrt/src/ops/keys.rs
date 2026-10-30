//! Asymmetric keys for `node:crypto` and WebCrypto: import/export (PEM, DER, JWK; PKCS#8,
//! SPKI, PKCS#1, SEC1, X.509 certificates, encrypted PKCS#8 and legacy encrypted PEM),
//! generation, signatures (RSA PKCS#1 v1.5 / PSS, ECDSA P-256/384/521 and secp256k1, Ed25519,
//! Ed448, DSA), RSA encryption, ECDH, X25519, X448 and finite-field Diffie-Hellman.
//!
//! Keys cross into JS as DER: private keys as PKCS#8, public keys as SPKI (the JS `KeyObject`
//! keeps the bytes; every op parses what it needs).

use base64::Engine as _;
use crate::js::{Context, JsArgs, JsResult, JsValue, js_string};
use der::asn1::ObjectIdentifier;
use der::Decode;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use pkcs8::{DecodePrivateKey, EncodePrivateKey};
use rand::RngCore;
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{BigUint, RsaPrivateKey, RsaPublicKey};
use spki::EncodePublicKey;

use super::cipher::CipherError;
use super::crypto::{digest_name, hash};
use super::{bytes_arg, bytes_to_js, object};
use crate::engine::{OpFn, arg_string, js_str};

type KResult<T> = Result<T, CipherError>;

const OID_RSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
const OID_RSA_PSS: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.10");
const OID_EC: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.2.1");
const OID_P256: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.3.1.7");
const OID_P384: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.132.0.34");
const OID_P521: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.132.0.35");
const OID_ED25519: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.112");
const OID_X25519: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.110");
const OID_X448: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.111");
const OID_ED448: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.113");
const OID_K256: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.132.0.10");
const OID_DSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10040.4.1");

fn unsupported() -> CipherError {
    CipherError::new("Error", "error:1E08010C:DECODER routines::unsupported", Some("ERR_OSSL_UNSUPPORTED"))
}

fn err(kind: &'static str, message: &str, code: Option<&'static str>) -> CipherError {
    CipherError::new(kind, message, code)
}

fn rng() -> rand::rngs::ThreadRng {
    rand::thread_rng()
}

// ---------------------------------------------------------------------------
// Key model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Curve {
    P256,
    P384,
    P521,
    K256,
}

impl Curve {
    pub(crate) fn from_name(name: &str) -> Option<Curve> {
        Some(match name {
            "prime256v1" | "P-256" | "secp256r1" => Curve::P256,
            "secp384r1" | "P-384" => Curve::P384,
            "secp521r1" | "P-521" => Curve::P521,
            "secp256k1" => Curve::K256,
            _ => return None,
        })
    }
    fn from_oid(oid: ObjectIdentifier) -> Option<Curve> {
        Some(match oid {
            OID_P256 => Curve::P256,
            OID_P384 => Curve::P384,
            OID_P521 => Curve::P521,
            OID_K256 => Curve::K256,
            _ => return None,
        })
    }
    fn openssl_name(self) -> &'static str {
        match self {
            Curve::P256 => "prime256v1",
            Curve::P384 => "secp384r1",
            Curve::P521 => "secp521r1",
            Curve::K256 => "secp256k1",
        }
    }
    fn jwk_name(self) -> &'static str {
        match self {
            Curve::P256 => "P-256",
            Curve::P384 => "P-384",
            Curve::P521 => "P-521",
            Curve::K256 => "secp256k1",
        }
    }
    fn field_len(self) -> usize {
        match self {
            Curve::P256 => 32,
            Curve::P384 => 48,
            Curve::P521 => 66,
            Curve::K256 => 32,
        }
    }
}

pub(crate) enum EcSecret {
    P256(p256::SecretKey),
    P384(p384::SecretKey),
    P521(p521::SecretKey),
    K256(k256::SecretKey),
}

#[derive(Clone)]
pub(crate) enum EcPublic {
    P256(p256::PublicKey),
    P384(p384::PublicKey),
    P521(p521::PublicKey),
    K256(k256::PublicKey),
}

/// Run `$body` with `$m` bound to the curve crate of an [`EcSecret`]/[`EcPublic`] value.
macro_rules! ec_dispatch {
    ($value:expr, $enum:ident, $k:ident, $m:ident => $body:expr) => {
        match $value {
            $enum::P256($k) => {
                #[allow(unused_imports)]
                use p256 as $m;
                $body
            }
            $enum::P384($k) => {
                #[allow(unused_imports)]
                use p384 as $m;
                $body
            }
            $enum::P521($k) => {
                #[allow(unused_imports)]
                use p521 as $m;
                $body
            }
            $enum::K256($k) => {
                #[allow(unused_imports)]
                use k256 as $m;
                $body
            }
        }
    };
}

/// Run `$body` with `$m` bound to the crate of a [`Curve`].
macro_rules! curve_dispatch {
    ($curve:expr, $m:ident => $body:expr) => {
        match $curve {
            Curve::P256 => {
                #[allow(unused_imports)]
                use p256 as $m;
                $body
            }
            Curve::P384 => {
                #[allow(unused_imports)]
                use p384 as $m;
                $body
            }
            Curve::P521 => {
                #[allow(unused_imports)]
                use p521 as $m;
                $body
            }
            Curve::K256 => {
                #[allow(unused_imports)]
                use k256 as $m;
                $body
            }
        }
    };
}

impl EcSecret {
    fn curve(&self) -> Curve {
        match self {
            EcSecret::P256(_) => Curve::P256,
            EcSecret::P384(_) => Curve::P384,
            EcSecret::P521(_) => Curve::P521,
            EcSecret::K256(_) => Curve::K256,
        }
    }
    fn from_bytes(curve: Curve, d: &[u8]) -> KResult<Self> {
        let bad = || err("Error", "Invalid private key", Some("ERR_CRYPTO_INVALID_KEYTYPE"));
        Ok(curve_dispatch!(curve, m => {
            // OpenSSH stores the scalar as an mpint (a leading zero byte when the high bit is set).
            let mut d = d;
            while d.len() > curve.field_len() && d[0] == 0 {
                d = &d[1..];
            }
            let mut padded = vec![0u8; curve.field_len().saturating_sub(d.len())];
            padded.extend_from_slice(d);
            let key = m::SecretKey::from_slice(&padded).map_err(|_| bad())?;
            Self::wrap_any(key)
        }))
    }
    fn public(&self) -> EcPublic {
        ec_dispatch!(self, EcSecret, k, _m => EcPublic::wrap_any(k.public_key()))
    }
    fn to_bytes(&self) -> Vec<u8> {
        ec_dispatch!(self, EcSecret, k, _m => k.to_bytes().to_vec())
    }
    fn to_pkcs8(&self) -> Vec<u8> {
        ec_dispatch!(self, EcSecret, k, _m => k.to_pkcs8_der().map(|d| d.as_bytes().to_vec()).unwrap_or_default())
    }
    /// SEC1 `ECPrivateKey` with the named-curve parameters and public key (like OpenSSL).
    fn to_sec1(&self) -> Vec<u8> {
        use der::Encode;
        let d = self.to_bytes();
        let point = self.public().to_sec1(false);
        let oid = match self.curve() {
            Curve::P256 => OID_P256,
            Curve::P384 => OID_P384,
            Curve::P521 => OID_P521,
            Curve::K256 => OID_K256,
        };
        let key = sec1::EcPrivateKey { private_key: &d, parameters: Some(sec1::EcParameters::NamedCurve(oid)), public_key: Some(&point) };
        key.to_der().unwrap_or_default()
    }
    fn generate(curve: Curve) -> Self {
        curve_dispatch!(curve, m => Self::wrap_any(m::SecretKey::random(&mut rng())))
    }
}

/// Conversions from the per-curve key types (used through the dispatch macros).
trait WrapEc<T> {
    fn wrap_any(value: T) -> Self;
}
impl WrapEc<p256::SecretKey> for EcSecret {
    fn wrap_any(v: p256::SecretKey) -> Self {
        EcSecret::P256(v)
    }
}
impl WrapEc<p384::SecretKey> for EcSecret {
    fn wrap_any(v: p384::SecretKey) -> Self {
        EcSecret::P384(v)
    }
}
impl WrapEc<p521::SecretKey> for EcSecret {
    fn wrap_any(v: p521::SecretKey) -> Self {
        EcSecret::P521(v)
    }
}
impl WrapEc<k256::SecretKey> for EcSecret {
    fn wrap_any(v: k256::SecretKey) -> Self {
        EcSecret::K256(v)
    }
}
impl WrapEc<k256::PublicKey> for EcPublic {
    fn wrap_any(v: k256::PublicKey) -> Self {
        EcPublic::K256(v)
    }
}
impl WrapEc<p256::PublicKey> for EcPublic {
    fn wrap_any(v: p256::PublicKey) -> Self {
        EcPublic::P256(v)
    }
}
impl WrapEc<p384::PublicKey> for EcPublic {
    fn wrap_any(v: p384::PublicKey) -> Self {
        EcPublic::P384(v)
    }
}
impl WrapEc<p521::PublicKey> for EcPublic {
    fn wrap_any(v: p521::PublicKey) -> Self {
        EcPublic::P521(v)
    }
}

impl EcPublic {
    fn curve(&self) -> Curve {
        match self {
            EcPublic::P256(_) => Curve::P256,
            EcPublic::P384(_) => Curve::P384,
            EcPublic::P521(_) => Curve::P521,
            EcPublic::K256(_) => Curve::K256,
        }
    }
    pub(crate) fn from_sec1(curve: Curve, bytes: &[u8]) -> KResult<Self> {
        let bad = || err("Error", "Public key is not valid for specified curve", Some("ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY"));
        Ok(curve_dispatch!(curve, m => Self::wrap_any(m::PublicKey::from_sec1_bytes(bytes).map_err(|_| bad())?)))
    }
    pub(crate) fn to_sec1(&self, compressed: bool) -> Vec<u8> {
        ec_dispatch!(self, EcPublic, k, _m => k.to_encoded_point(compressed).as_bytes().to_vec())
    }
    fn to_spki(&self) -> Vec<u8> {
        ec_dispatch!(self, EcPublic, k, _m => k.to_public_key_der().map(|d| d.as_bytes().to_vec()).unwrap_or_default())
    }
}

pub(crate) enum PrivKey {
    /// RSA key; `true` for an RSASSA-PSS key (`rsa-pss`).
    Rsa(Box<RsaPrivateKey>, bool),
    Ec(EcSecret),
    Ed25519([u8; 32]),
    X25519([u8; 32]),
    Ed448(Box<[u8; 57]>),
    X448(Box<[u8; 56]>),
    Dsa(Box<dsa::SigningKey>),
}

pub(crate) enum PubKey {
    Rsa(Box<RsaPublicKey>, bool),
    Ec(EcPublic),
    Ed25519([u8; 32]),
    X25519([u8; 32]),
    Ed448(Box<[u8; 57]>),
    X448(Box<[u8; 56]>),
    Dsa(Box<dsa::VerifyingKey>),
}

/// PKCS#8 for an OKP key (RFC 8410): the private key is an OCTET STRING in an OCTET STRING.
fn okp_pkcs8(oid_last: u8, seed: &[u8]) -> Vec<u8> {
    let n = seed.len() as u8;
    let mut v = vec![0x30, n + 14, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, oid_last, 0x04, n + 2, 0x04, n];
    v.extend_from_slice(seed);
    v
}

fn okp_spki(oid_last: u8, public: &[u8]) -> Vec<u8> {
    let n = public.len() as u8;
    let mut v = vec![0x30, n + 10, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, oid_last, 0x03, n + 1, 0x00];
    v.extend_from_slice(public);
    v
}

fn arr32(b: &[u8]) -> KResult<[u8; 32]> {
    b.try_into().map_err(|_| unsupported())
}

fn arr<const N: usize>(b: &[u8]) -> KResult<Box<[u8; N]>> {
    let a: [u8; N] = b.try_into().map_err(|_| unsupported())?;
    Ok(Box::new(a))
}

impl PrivKey {
    pub(crate) fn asym(&self) -> &'static str {
        match self {
            PrivKey::Rsa(_, false) => "rsa",
            PrivKey::Rsa(_, true) => "rsa-pss",
            PrivKey::Ec(_) => "ec",
            PrivKey::Ed25519(_) => "ed25519",
            PrivKey::X25519(_) => "x25519",
            PrivKey::Ed448(_) => "ed448",
            PrivKey::X448(_) => "x448",
            PrivKey::Dsa(_) => "dsa",
        }
    }

    pub(crate) fn from_pkcs8(der: &[u8]) -> KResult<Self> {
        let pki = pkcs8::PrivateKeyInfo::try_from(der).map_err(|_| unsupported())?;
        let oid = pki.algorithm.oid;
        match oid {
            OID_RSA | OID_RSA_PSS => {
                use pkcs1::DecodeRsaPrivateKey;
                let k = RsaPrivateKey::from_pkcs1_der(pki.private_key).map_err(|_| unsupported())?;
                Ok(PrivKey::Rsa(Box::new(k), oid == OID_RSA_PSS))
            }
            OID_EC => {
                let curve = pki
                    .algorithm
                    .parameters
                    .and_then(|p| p.decode_as::<ObjectIdentifier>().ok())
                    .and_then(Curve::from_oid)
                    .ok_or_else(unsupported)?;
                Ok(PrivKey::Ec(curve_dispatch!(curve, m => EcSecret::wrap_any(m::SecretKey::from_pkcs8_der(der).map_err(|_| unsupported())?))))
            }
            OID_ED25519 | OID_X25519 => {
                // The private key is an OCTET STRING wrapping the 32-byte seed.
                let inner = der::asn1::OctetStringRef::from_der(pki.private_key).map_err(|_| unsupported())?;
                let seed = arr32(inner.as_bytes())?;
                Ok(if oid == OID_ED25519 { PrivKey::Ed25519(seed) } else { PrivKey::X25519(seed) })
            }
            OID_ED448 | OID_X448 => {
                let inner = der::asn1::OctetStringRef::from_der(pki.private_key).map_err(|_| unsupported())?;
                Ok(if oid == OID_ED448 { PrivKey::Ed448(arr(inner.as_bytes())?) } else { PrivKey::X448(arr(inner.as_bytes())?) })
            }
            OID_DSA => Ok(PrivKey::Dsa(Box::new(dsa::SigningKey::try_from(pki).map_err(|_| unsupported())?))),
            _ => Err(unsupported()),
        }
    }

    pub(crate) fn to_pkcs8(&self) -> Vec<u8> {
        match self {
            PrivKey::Rsa(k, _) => k.to_pkcs8_der().map(|d| d.as_bytes().to_vec()).unwrap_or_default(),
            PrivKey::Ec(k) => k.to_pkcs8(),
            PrivKey::Ed25519(s) => okp_pkcs8(0x70, s),
            PrivKey::X25519(s) => okp_pkcs8(0x6e, s),
            PrivKey::Ed448(s) => okp_pkcs8(0x71, &s[..]),
            PrivKey::X448(s) => okp_pkcs8(0x6f, &s[..]),
            PrivKey::Dsa(k) => k.to_pkcs8_der().map(|d| d.as_bytes().to_vec()).unwrap_or_default(),
        }
    }

    pub(crate) fn public(&self) -> PubKey {
        match self {
            PrivKey::Rsa(k, pss) => PubKey::Rsa(Box::new(k.to_public_key()), *pss),
            PrivKey::Ec(k) => PubKey::Ec(k.public()),
            PrivKey::Ed25519(s) => PubKey::Ed25519(ed25519_dalek::SigningKey::from_bytes(s).verifying_key().to_bytes()),
            PrivKey::X25519(s) => PubKey::X25519(x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*s)).to_bytes()),
            PrivKey::Ed448(s) => PubKey::Ed448(Box::new(super::ed448::ed448_public(s))),
            PrivKey::X448(s) => PubKey::X448(Box::new(super::ed448::x448_public(s))),
            PrivKey::Dsa(k) => PubKey::Dsa(Box::new(k.verifying_key().clone())),
        }
    }
}

impl PubKey {
    pub(crate) fn asym(&self) -> &'static str {
        match self {
            PubKey::Rsa(_, false) => "rsa",
            PubKey::Rsa(_, true) => "rsa-pss",
            PubKey::Ec(_) => "ec",
            PubKey::Ed25519(_) => "ed25519",
            PubKey::X25519(_) => "x25519",
            PubKey::Ed448(_) => "ed448",
            PubKey::X448(_) => "x448",
            PubKey::Dsa(_) => "dsa",
        }
    }

    pub(crate) fn from_spki(der: &[u8]) -> KResult<Self> {
        let spki = spki::SubjectPublicKeyInfoRef::try_from(der).map_err(|_| unsupported())?;
        let oid = spki.algorithm.oid;
        let bits = spki.subject_public_key.raw_bytes();
        match oid {
            OID_RSA | OID_RSA_PSS => {
                use pkcs1::DecodeRsaPublicKey;
                let k = RsaPublicKey::from_pkcs1_der(bits).map_err(|_| unsupported())?;
                Ok(PubKey::Rsa(Box::new(k), oid == OID_RSA_PSS))
            }
            OID_EC => {
                let curve = spki
                    .algorithm
                    .parameters
                    .and_then(|p| p.decode_as::<ObjectIdentifier>().ok())
                    .and_then(Curve::from_oid)
                    .ok_or_else(unsupported)?;
                Ok(PubKey::Ec(EcPublic::from_sec1(curve, bits).map_err(|_| unsupported())?))
            }
            OID_ED25519 => Ok(PubKey::Ed25519(arr32(bits)?)),
            OID_X25519 => Ok(PubKey::X25519(arr32(bits)?)),
            OID_ED448 => Ok(PubKey::Ed448(arr(bits)?)),
            OID_X448 => Ok(PubKey::X448(arr(bits)?)),
            OID_DSA => Ok(PubKey::Dsa(Box::new(dsa::VerifyingKey::try_from(spki).map_err(|_| unsupported())?))),
            _ => Err(unsupported()),
        }
    }

    pub(crate) fn to_spki(&self) -> Vec<u8> {
        match self {
            PubKey::Rsa(k, _) => k.to_public_key_der().map(|d| d.as_bytes().to_vec()).unwrap_or_default(),
            PubKey::Ec(k) => k.to_spki(),
            PubKey::Ed25519(p) => okp_spki(0x70, p),
            PubKey::X25519(p) => okp_spki(0x6e, p),
            PubKey::Ed448(p) => okp_spki(0x71, &p[..]),
            PubKey::X448(p) => okp_spki(0x6f, &p[..]),
            PubKey::Dsa(k) => k.to_public_key_der().map(|d| d.as_bytes().to_vec()).unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// PEM and DER helpers
// ---------------------------------------------------------------------------

/// A PEM block: label, headers (`Proc-Type`, `DEK-Info`) and body.
pub(crate) struct Pem {
    pub label: String,
    pub headers: Vec<(String, String)>,
    pub der: Vec<u8>,
}

/// Parse every PEM block in `text`.
pub(crate) fn parse_pem_all(text: &str) -> Vec<Pem> {
    let mut out = Vec::new();
    let mut lines = text.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(label) = line.strip_prefix("-----BEGIN ").and_then(|l| l.strip_suffix("-----")) else { continue };
        let mut headers = Vec::new();
        let mut body = String::new();
        for l in lines.by_ref() {
            if l.starts_with("-----END ") {
                break;
            }
            if body.is_empty() && l.contains(':') {
                if let Some((k, v)) = l.split_once(':') {
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
                continue;
            }
            body.push_str(l);
        }
        if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(body.as_bytes()) {
            out.push(Pem { label: label.to_string(), headers, der });
        }
    }
    out
}

pub(crate) fn to_pem(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut s = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        s.push('\n');
    }
    s.push_str(&format!("-----END {label}-----\n"));
    s
}

/// One DER element: tag, the whole TLV, and the content.
pub(crate) struct Tlv<'a> {
    pub tag: u8,
    pub raw: &'a [u8],
    pub content: &'a [u8],
}

/// Read one TLV from the front of `b`.
pub(crate) fn read_tlv(b: &[u8]) -> Option<(Tlv<'_>, &[u8])> {
    let tag = *b.first()?;
    let first = *b.get(1)? as usize;
    let (len, hdr) = if first < 0x80 {
        (first, 2)
    } else {
        let n = first & 0x7f;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *b.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let end = hdr.checked_add(len)?;
    if end > b.len() {
        return None;
    }
    Some((Tlv { tag, raw: &b[..end], content: &b[hdr..end] }, &b[end..]))
}

/// The children of a constructed DER element.
pub(crate) fn children(content: &[u8]) -> Vec<Tlv<'_>> {
    let mut out = Vec::new();
    let mut rest = content;
    while let Some((t, r)) = read_tlv(rest) {
        out.push(t);
        rest = r;
    }
    out
}

/// Fields of an X.509 certificate's TBSCertificate: (serial, issuer, validity, subject, spki).
pub(crate) fn cert_fields(der: &[u8]) -> Option<[&[u8]; 5]> {
    let (cert, _) = read_tlv(der)?;
    let parts = children(cert.content);
    let tbs = parts.first()?;
    let mut fields = children(tbs.content);
    if fields.first()?.tag == 0xa0 {
        fields.remove(0);
    }
    if fields.len() < 6 {
        return None;
    }
    Some([fields[0].content, fields[2].raw, fields[3].raw, fields[4].raw, fields[5].raw])
}

/// The subjectAltName extension formatted like OpenSSL/Node (`DNS:a, IP Address:1.2.3.4`).
pub(crate) fn subject_alt_names(der: &[u8]) -> Option<String> {
    let (cert, _) = read_tlv(der)?;
    let tbs = children(cert.content).into_iter().next()?;
    let ext = children(tbs.content).into_iter().find(|f| f.tag == 0xa3)?;
    let (seq, _) = read_tlv(ext.content)?;
    for e in children(seq.content) {
        let parts = children(e.content);
        if parts.first().map(|p| p.content) != Some(&[0x55, 0x1d, 0x11][..]) {
            continue;
        }
        let value = parts.last()?;
        let (names, _) = read_tlv(value.content)?;
        let mut out = Vec::new();
        for n in children(names.content) {
            match n.tag {
                0x82 => out.push(format!("DNS:{}", String::from_utf8_lossy(n.content))),
                0x81 => out.push(format!("email:{}", String::from_utf8_lossy(n.content))),
                0x86 => out.push(format!("URI:{}", String::from_utf8_lossy(n.content))),
                0x87 if n.content.len() == 4 => out.push(format!("IP Address:{}", std::net::Ipv4Addr::from(<[u8; 4]>::try_from(n.content).ok()?))),
                0x87 if n.content.len() == 16 => {
                    let ip = std::net::Ipv6Addr::from(<[u8; 16]>::try_from(n.content).ok()?);
                    out.push(format!("IP Address:{}", ip.segments().iter().map(|s| format!("{s:X}")).collect::<Vec<_>>().join(":")));
                }
                _ => {}
            }
        }
        return Some(out.join(", "));
    }
    None
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// What `keyImport` produced.
pub(crate) enum Imported {
    Private(PrivKey),
    Public(PubKey),
}

/// OpenSSL tries to prompt for the passphrase and fails without a terminal (what Node reports).
fn missing_passphrase() -> CipherError {
    err("Error", "error:07880109:common libcrypto routines::interrupted or cancelled", Some("ERR_OSSL_CRYPTO_INTERRUPTED_OR_CANCELLED"))
}

fn bad_decrypt() -> CipherError {
    err("Error", "error:1C800064:Provider routines::bad decrypt", Some("ERR_OSSL_BAD_DECRYPT"))
}

/// OpenSSL's `EVP_BytesToKey` with MD5 and one iteration (legacy PEM encryption).
fn bytes_to_key(pass: &[u8], salt: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev: Vec<u8> = Vec::new();
    while out.len() < len {
        let mut input = prev.clone();
        input.extend_from_slice(pass);
        input.extend_from_slice(&salt[..8.min(salt.len())]);
        prev = hash("md5", &input).unwrap_or_default();
        out.extend_from_slice(&prev);
    }
    out.truncate(len);
    out
}

fn legacy_cipher_name(dek: &str) -> Option<(&'static str, usize)> {
    Some(match dek.to_ascii_uppercase().as_str() {
        "AES-128-CBC" => ("aes-128-cbc", 16),
        "AES-192-CBC" => ("aes-192-cbc", 24),
        "AES-256-CBC" => ("aes-256-cbc", 32),
        "DES-EDE3-CBC" => ("des-ede3-cbc", 24),
        _ => return None,
    })
}

fn decrypt_legacy_pem(pem: &Pem, passphrase: Option<&[u8]>) -> KResult<Vec<u8>> {
    let encrypted = pem.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("Proc-Type") && v.contains("ENCRYPTED"));
    if !encrypted {
        return Ok(pem.der.clone());
    }
    let pass = passphrase.ok_or_else(missing_passphrase)?;
    let dek = pem.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("DEK-Info")).map(|(_, v)| v.as_str()).ok_or_else(unsupported)?;
    let (alg, iv_hex) = dek.split_once(',').ok_or_else(unsupported)?;
    let (cipher, key_len) = legacy_cipher_name(alg.trim()).ok_or_else(unsupported)?;
    let iv: Vec<u8> = (0..iv_hex.trim().len() / 2).filter_map(|i| u8::from_str_radix(&iv_hex.trim()[2 * i..2 * i + 2], 16).ok()).collect();
    let key = bytes_to_key(pass, &iv, key_len);
    super::cipher::one_shot(cipher, &key, Some(&iv), false, &pem.der).map_err(|_| bad_decrypt())
}

fn encrypt_legacy_pem(label: &str, der: &[u8], cipher: &str, pass: &[u8]) -> KResult<String> {
    let (name, key_len) = legacy_cipher_name(cipher).ok_or_else(|| err("Error", "Unknown cipher", Some("ERR_CRYPTO_UNKNOWN_CIPHER")))?;
    let iv_len = if name.starts_with("des") { 8 } else { 16 };
    let mut iv = vec![0u8; iv_len];
    rng().fill_bytes(&mut iv);
    let key = bytes_to_key(pass, &iv, key_len);
    let ct = super::cipher::one_shot(name, &key, Some(&iv), true, der)?;
    let iv_hex: String = iv.iter().map(|b| format!("{b:02X}")).collect();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&ct);
    let mut s = format!("-----BEGIN {label}-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: {},{iv_hex}\n\n", name.to_ascii_uppercase());
    for chunk in b64.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        s.push('\n');
    }
    s.push_str(&format!("-----END {label}-----\n"));
    Ok(s)
}

fn private_from_der(der: &[u8], typ: &str, passphrase: Option<&[u8]>) -> KResult<PrivKey> {
    match typ {
        "pkcs1" => {
            use pkcs1::DecodeRsaPrivateKey;
            Ok(PrivKey::Rsa(Box::new(RsaPrivateKey::from_pkcs1_der(der).map_err(|_| unsupported())?), false))
        }
        "sec1" => {
            let ec = sec1::EcPrivateKey::from_der(der).map_err(|_| unsupported())?;
            let by_len = match ec.private_key.len() {
                32 => Some(Curve::P256),
                48 => Some(Curve::P384),
                65 | 66 => Some(Curve::P521),
                _ => None,
            };
            let curve = ec.parameters.and_then(|p| p.named_curve()).and_then(Curve::from_oid).or(by_len).ok_or_else(unsupported)?;
            Ok(PrivKey::Ec(EcSecret::from_bytes(curve, ec.private_key)?))
        }
        _ => {
            if let Ok(enc) = pkcs8::EncryptedPrivateKeyInfo::try_from(der) {
                let pass = passphrase.ok_or_else(missing_passphrase)?;
                let doc = enc.decrypt(pass).map_err(|_| bad_decrypt())?;
                return PrivKey::from_pkcs8(doc.as_bytes());
            }
            PrivKey::from_pkcs8(der)
                .or_else(|_| private_from_der(der, "pkcs1", None))
                .or_else(|_| private_from_der(der, "sec1", None))
        }
    }
}

fn public_from_der(der: &[u8], typ: &str) -> KResult<PubKey> {
    match typ {
        "pkcs1" => {
            use pkcs1::DecodeRsaPublicKey;
            Ok(PubKey::Rsa(Box::new(RsaPublicKey::from_pkcs1_der(der).map_err(|_| unsupported())?), false))
        }
        _ => PubKey::from_spki(der).or_else(|_| {
            let [_, _, _, _, spki] = cert_fields(der).ok_or_else(unsupported)?;
            PubKey::from_spki(spki)
        }),
    }
}

/// Import key material. `want_private` selects createPrivateKey vs createPublicKey semantics
/// (a private key given to createPublicKey yields its public key).
pub(crate) fn import(data: &[u8], format: &str, typ: &str, passphrase: Option<&[u8]>, want_private: bool) -> KResult<Imported> {
    if format == "der" {
        return if want_private {
            Ok(Imported::Private(private_from_der(data, typ, passphrase)?))
        } else if typ == "pkcs1" || typ == "spki" {
            Ok(Imported::Public(public_from_der(data, typ)?))
        } else {
            match private_from_der(data, typ, passphrase) {
                Ok(k) => Ok(Imported::Public(k.public())),
                Err(_) => Ok(Imported::Public(public_from_der(data, typ)?)),
            }
        };
    }
    let text = String::from_utf8_lossy(data);
    let blocks = parse_pem_all(&text);
    let Some(pem) = blocks.into_iter().find(|p| !p.label.contains("PARAMETERS")) else {
        return Err(unsupported());
    };
    let private = match pem.label.as_str() {
        "PRIVATE KEY" => Some(PrivKey::from_pkcs8(&pem.der)?),
        "ENCRYPTED PRIVATE KEY" => Some(private_from_der(&pem.der, "pkcs8", passphrase)?),
        "RSA PRIVATE KEY" => Some(private_from_der(&decrypt_legacy_pem(&pem, passphrase)?, "pkcs1", None)?),
        "EC PRIVATE KEY" => Some(private_from_der(&decrypt_legacy_pem(&pem, passphrase)?, "sec1", None)?),
        _ => None,
    };
    if let Some(k) = private {
        return Ok(if want_private { Imported::Private(k) } else { Imported::Public(k.public()) });
    }
    if want_private {
        return Err(unsupported());
    }
    let public = match pem.label.as_str() {
        "PUBLIC KEY" => PubKey::from_spki(&pem.der).map_err(|_| err("Error", "error:068000A8:asn1 encoding routines::wrong tag", Some("ERR_OSSL_ASN1_WRONG_TAG")))?,
        "RSA PUBLIC KEY" => public_from_der(&pem.der, "pkcs1")?,
        "CERTIFICATE" | "X509 CERTIFICATE" | "TRUSTED CERTIFICATE" => {
            let [_, _, _, _, spki] = cert_fields(&pem.der).ok_or_else(unsupported)?;
            PubKey::from_spki(spki)?
        }
        _ => return Err(unsupported()),
    };
    Ok(Imported::Public(public))
}

fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn unb64url(s: &str) -> KResult<Vec<u8>> {
    let s = s.trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.replace('+', "-").replace('/', "_"))
        .map_err(|_| err("TypeError", "Invalid JWK data", Some("ERR_CRYPTO_INVALID_JWK")))
}

/// Import a JWK (`kty` RSA, EC or OKP).
pub(crate) fn import_jwk(jwk: &serde_json::Value, want_private: bool) -> KResult<Imported> {
    let invalid = |what: &str| err("TypeError", &format!("Invalid JWK {what}"), Some("ERR_CRYPTO_INVALID_JWK"));
    let field = |name: &str| -> KResult<Option<Vec<u8>>> { jwk.get(name).and_then(|v| v.as_str()).map(unb64url).transpose() };
    let req = |name: &str| -> KResult<Vec<u8>> { field(name)?.ok_or_else(|| invalid(&format!("{name} Parameter"))) };
    let kty = jwk.get("kty").and_then(|v| v.as_str()).unwrap_or_default();
    let has_d = jwk.get("d").is_some();
    if want_private && !has_d {
        return Err(invalid("private key"));
    }
    match kty {
        "RSA" => {
            let n = BigUint::from_bytes_be(&req("n")?);
            let e = BigUint::from_bytes_be(&req("e")?);
            if has_d {
                let d = BigUint::from_bytes_be(&req("d")?);
                let primes = match (field("p")?, field("q")?) {
                    (Some(p), Some(q)) => vec![BigUint::from_bytes_be(&p), BigUint::from_bytes_be(&q)],
                    _ => Vec::new(),
                };
                let mut k = RsaPrivateKey::from_components(n, e, d, primes).map_err(|_| invalid("RSA key"))?;
                let _ = k.precompute();
                let k = PrivKey::Rsa(Box::new(k), false);
                Ok(if want_private { Imported::Private(k) } else { Imported::Public(k.public()) })
            } else {
                Ok(Imported::Public(PubKey::Rsa(Box::new(RsaPublicKey::new(n, e).map_err(|_| invalid("RSA key"))?), false)))
            }
        }
        "EC" => {
            let crv = jwk.get("crv").and_then(|v| v.as_str()).unwrap_or_default();
            let curve = Curve::from_name(crv).ok_or_else(|| err("TypeError", "Unsupported JWK EC curve: ".to_string().as_str(), Some("ERR_CRYPTO_INVALID_JWK")))?;
            if has_d {
                let k = PrivKey::Ec(EcSecret::from_bytes(curve, &req("d")?)?);
                return Ok(if want_private { Imported::Private(k) } else { Imported::Public(k.public()) });
            }
            let mut point = vec![4u8];
            let pad = |mut v: Vec<u8>| {
                while v.len() < curve.field_len() {
                    v.insert(0, 0);
                }
                v
            };
            point.extend(pad(req("x")?));
            point.extend(pad(req("y")?));
            Ok(Imported::Public(PubKey::Ec(EcPublic::from_sec1(curve, &point).map_err(|_| invalid("EC key"))?)))
        }
        "OKP" => {
            let crv = jwk.get("crv").and_then(|v| v.as_str()).unwrap_or_default();
            if crv == "Ed448" || crv == "X448" {
                let ed = crv == "Ed448";
                if has_d {
                    let d = req("d")?;
                    let k = if ed { PrivKey::Ed448(arr(&d).map_err(|_| invalid("OKP key"))?) } else { PrivKey::X448(arr(&d).map_err(|_| invalid("OKP key"))?) };
                    return Ok(if want_private { Imported::Private(k) } else { Imported::Public(k.public()) });
                }
                let x = req("x")?;
                return Ok(Imported::Public(if ed { PubKey::Ed448(arr(&x).map_err(|_| invalid("OKP key"))?) } else { PubKey::X448(arr(&x).map_err(|_| invalid("OKP key"))?) }));
            }
            let ed = match crv {
                "Ed25519" => true,
                "X25519" => false,
                _ => return Err(err("TypeError", "Unsupported JWK OKP curve", Some("ERR_CRYPTO_INVALID_JWK"))),
            };
            if has_d {
                let d = arr32(&req("d")?).map_err(|_| invalid("OKP key"))?;
                let k = if ed { PrivKey::Ed25519(d) } else { PrivKey::X25519(d) };
                return Ok(if want_private { Imported::Private(k) } else { Imported::Public(k.public()) });
            }
            let x = arr32(&req("x")?).map_err(|_| invalid("OKP key"))?;
            Ok(Imported::Public(if ed { PubKey::Ed25519(x) } else { PubKey::X25519(x) }))
        }
        _ => Err(err("TypeError", "Invalid JWK \"kty\" Parameter", Some("ERR_CRYPTO_INVALID_JWK"))),
    }
}

/// Raw public keys (WebCrypto `raw` format): EC points, Ed25519/X25519 bytes.
pub(crate) fn import_raw_public(asym: &str, curve: Option<&str>, raw: &[u8]) -> KResult<PubKey> {
    match asym {
        "ec" => Ok(PubKey::Ec(EcPublic::from_sec1(Curve::from_name(curve.unwrap_or_default()).ok_or_else(unsupported)?, raw)?)),
        "ed25519" => Ok(PubKey::Ed25519(arr32(raw)?)),
        "x25519" => Ok(PubKey::X25519(arr32(raw)?)),
        "ed448" => Ok(PubKey::Ed448(arr(raw)?)),
        "x448" => Ok(PubKey::X448(arr(raw)?)),
        _ => Err(unsupported()),
    }
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

pub(crate) enum Exported {
    Text(String),
    Bytes(Vec<u8>),
    Json(serde_json::Value),
}

fn big(b: &BigUint) -> String {
    b64url(&b.to_bytes_be())
}

fn rsa_jwk_public(k: &RsaPublicKey) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert("kty".into(), "RSA".into());
    m.insert("n".into(), big(k.n()).into());
    m.insert("e".into(), big(k.e()).into());
    m
}

fn ec_jwk_public(k: &EcPublic) -> serde_json::Map<String, serde_json::Value> {
    let point = k.to_sec1(false);
    let n = k.curve().field_len();
    let mut m = serde_json::Map::new();
    m.insert("kty".into(), "EC".into());
    m.insert("x".into(), b64url(&point[1..1 + n]).into());
    m.insert("y".into(), b64url(&point[1 + n..]).into());
    m.insert("crv".into(), k.curve().jwk_name().into());
    m
}

pub(crate) fn export_public(k: &PubKey, format: &str, typ: &str) -> KResult<Exported> {
    if format == "jwk" {
        let m = match k {
            PubKey::Rsa(r, _) => rsa_jwk_public(r),
            PubKey::Ec(e) => ec_jwk_public(e),
            PubKey::Ed25519(_) | PubKey::X25519(_) | PubKey::Ed448(_) | PubKey::X448(_) => {
                let (crv, x): (&str, &[u8]) = match k {
                    PubKey::Ed25519(p) => ("Ed25519", p),
                    PubKey::X25519(p) => ("X25519", p),
                    PubKey::Ed448(p) => ("Ed448", &p[..]),
                    PubKey::X448(p) => ("X448", &p[..]),
                    _ => unreachable!(),
                };
                let mut m = serde_json::Map::new();
                m.insert("crv".into(), crv.into());
                m.insert("x".into(), b64url(x).into());
                m.insert("kty".into(), "OKP".into());
                m
            }
            PubKey::Dsa(_) => return Err(err("Error", "Unsupported JWK Key Type.", Some("ERR_CRYPTO_JWK_UNSUPPORTED_KEY_TYPE"))),
        };
        return Ok(Exported::Json(serde_json::Value::Object(m)));
    }
    let (label, der) = match typ {
        "pkcs1" => match k {
            PubKey::Rsa(r, _) => {
                use pkcs1::EncodeRsaPublicKey;
                ("RSA PUBLIC KEY", r.to_pkcs1_der().map_err(|_| unsupported())?.as_bytes().to_vec())
            }
            _ => return Err(err("Error", "error:1E08010C:DECODER routines::unsupported", Some("ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS"))),
        },
        _ => ("PUBLIC KEY", k.to_spki()),
    };
    Ok(if format == "pem" { Exported::Text(to_pem(label, &der)) } else { Exported::Bytes(der) })
}

pub(crate) fn export_private(k: &PrivKey, format: &str, typ: &str, cipher: Option<&str>, passphrase: Option<&[u8]>) -> KResult<Exported> {
    if format == "jwk" {
        let mut m = match k {
            PrivKey::Rsa(r, _) => {
                let mut m = rsa_jwk_public(&r.to_public_key());
                m.insert("d".into(), big(r.d()).into());
                let primes = r.primes();
                if primes.len() >= 2 {
                    m.insert("p".into(), big(&primes[0]).into());
                    m.insert("q".into(), big(&primes[1]).into());
                }
                if let (Some(dp), Some(dq)) = (r.dp(), r.dq()) {
                    m.insert("dp".into(), big(dp).into());
                    m.insert("dq".into(), big(dq).into());
                }
                if let Some(qi) = r.crt_coefficient() {
                    m.insert("qi".into(), big(&qi).into());
                }
                m
            }
            PrivKey::Ec(e) => {
                let mut m = ec_jwk_public(&e.public());
                m.insert("d".into(), b64url(&e.to_bytes()).into());
                m
            }
            PrivKey::Ed25519(_) | PrivKey::X25519(_) | PrivKey::Ed448(_) | PrivKey::X448(_) => {
                let d: &[u8] = match k {
                    PrivKey::Ed25519(s) | PrivKey::X25519(s) => s,
                    PrivKey::Ed448(s) => &s[..],
                    PrivKey::X448(s) => &s[..],
                    _ => unreachable!(),
                };
                let Exported::Json(serde_json::Value::Object(mut m)) = export_public(&k.public(), "jwk", "")? else { unreachable!() };
                m.insert("d".into(), b64url(d).into());
                m
            }
            PrivKey::Dsa(_) => return Err(err("Error", "Unsupported JWK Key Type.", Some("ERR_CRYPTO_JWK_UNSUPPORTED_KEY_TYPE"))),
        };
        // Node orders RSA members n, e, d, p, q, dp, dq, qi and EC members kty, x, y, crv, d.
        m.sort_keys_like_node();
        return Ok(Exported::Json(serde_json::Value::Object(m)));
    }
    let (label, der) = match typ {
        "pkcs1" => match k {
            PrivKey::Rsa(r, _) => {
                use pkcs1::EncodeRsaPrivateKey;
                ("RSA PRIVATE KEY", r.to_pkcs1_der().map_err(|_| unsupported())?.as_bytes().to_vec())
            }
            _ => return Err(err("Error", "error:1E08010C:DECODER routines::unsupported", Some("ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS"))),
        },
        "sec1" => match k {
            PrivKey::Ec(e) => ("EC PRIVATE KEY", e.to_sec1()),
            _ => return Err(err("Error", "error:1E08010C:DECODER routines::unsupported", Some("ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS"))),
        },
        _ => ("PRIVATE KEY", k.to_pkcs8()),
    };
    if let (Some(cipher), Some(pass)) = (cipher, passphrase) {
        if typ == "pkcs8" {
            let mut salt = [0u8; 16];
            let mut iv = [0u8; 16];
            rng().fill_bytes(&mut salt);
            rng().fill_bytes(&mut iv);
            let params = pkcs8::pkcs5::pbes2::Parameters::pbkdf2_sha256_aes256cbc(2048, &salt, &iv).map_err(|_| unsupported())?;
            let pki = pkcs8::PrivateKeyInfo::try_from(der.as_slice()).map_err(|_| unsupported())?;
            let enc = pki.encrypt_with_params(params, pass).map_err(|_| unsupported())?;
            let bytes = enc.as_bytes().to_vec();
            return Ok(if format == "pem" { Exported::Text(to_pem("ENCRYPTED PRIVATE KEY", &bytes)) } else { Exported::Bytes(bytes) });
        }
        if format != "pem" {
            return Err(err("TypeError", "The selected key encoding pkcs1 does not support encryption.", Some("ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS")));
        }
        return Ok(Exported::Text(encrypt_legacy_pem(label, &der, cipher, pass)?));
    }
    Ok(if format == "pem" { Exported::Text(to_pem(label, &der)) } else { Exported::Bytes(der) })
}

trait SortLikeNode {
    fn sort_keys_like_node(&mut self);
}

impl SortLikeNode for serde_json::Map<String, serde_json::Value> {
    fn sort_keys_like_node(&mut self) {
        let order = ["kty", "n", "e", "d", "p", "q", "dp", "dq", "qi", "crv", "x", "y"];
        let mut entries: Vec<_> = std::mem::take(self).into_iter().collect();
        let rank = |k: &str, kty: &str| -> usize {
            // EC/OKP JWKs from Node: kty, x, y, crv, d (EC); crv, d, x, kty (OKP).
            match kty {
                "EC" => ["kty", "x", "y", "crv", "d"].iter().position(|o| *o == k).unwrap_or(99),
                "OKP" => ["crv", "d", "x", "kty"].iter().position(|o| *o == k).unwrap_or(99),
                _ => order.iter().position(|o| *o == k).unwrap_or(99),
            }
        };
        let kty = entries.iter().find(|(k, _)| k == "kty").and_then(|(_, v)| v.as_str().map(String::from)).unwrap_or_default();
        entries.sort_by_key(|(k, _)| rank(k, &kty));
        self.extend(entries);
    }
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

pub(crate) fn generate(asym: &str, modulus_bits: usize, exponent: u64, curve: Option<&str>, divisor_bits: Option<usize>) -> KResult<PrivKey> {
    match asym {
        "rsa" | "rsa-pss" => {
            if !(512..=16384).contains(&modulus_bits) {
                return Err(err("RangeError", "The value of \"options.modulusLength\" is out of range.", Some("ERR_OUT_OF_RANGE")));
            }
            let k = RsaPrivateKey::new_with_exp(&mut rng(), modulus_bits, &BigUint::from(exponent)).map_err(|e| err("Error", &e.to_string(), None))?;
            Ok(PrivKey::Rsa(Box::new(k), asym == "rsa-pss"))
        }
        "ec" => {
            let name = curve.unwrap_or_default();
            let curve = Curve::from_name(name).ok_or_else(|| err("TypeError", "Invalid EC curve name", Some("ERR_CRYPTO_INVALID_CURVE")))?;
            Ok(PrivKey::Ec(EcSecret::generate(curve)))
        }
        "ed25519" | "x25519" => {
            let mut seed = [0u8; 32];
            rng().fill_bytes(&mut seed);
            Ok(if asym == "ed25519" { PrivKey::Ed25519(seed) } else { PrivKey::X25519(seed) })
        }
        "ed448" => {
            let mut seed = [0u8; 57];
            rng().fill_bytes(&mut seed);
            Ok(PrivKey::Ed448(Box::new(seed)))
        }
        "x448" => {
            let mut seed = [0u8; 56];
            rng().fill_bytes(&mut seed);
            Ok(PrivKey::X448(Box::new(seed)))
        }
        "dsa" => {
            let n = divisor_bits.unwrap_or(if modulus_bits >= 3072 { 256 } else { 224 });
            Ok(PrivKey::Dsa(Box::new(dsa_generate(modulus_bits, n)?)))
        }
        _ => Err(err(
            "TypeError",
            &format!("The argument 'type' must be a supported key type. Received '{asym}'"),
            Some("ERR_INVALID_ARG_VALUE"),
        )),
    }
}

/// DSA domain parameters of any size (FIPS 186-4 style: `q` an N-bit prime, `p = kq + 1` an
/// L-bit prime, `g = h^((p-1)/q)`) and a key pair.
fn dsa_generate(l: usize, n: usize) -> KResult<dsa::SigningKey> {
    use num_traits::One;
    let failed = || err("Error", "error:0A000086:dsa routines::bad ffc parameters", None);
    if n < 2 || l <= n + 1 {
        return Err(failed());
    }
    let q = super::prime::generate(n, false, None, None)?;
    let two_q = &q << 1usize;
    let top = BigUint::one() << (l - 1);
    let p = loop {
        let mut x = BigUint::from_bytes_be(&{
            let mut b = vec![0u8; l.div_ceil(8)];
            rng().fill_bytes(&mut b);
            b
        }) % (&top << 1usize);
        x |= &top;
        let c = &x % &two_q;
        let p = x - c + BigUint::one();
        if p.bits() == l && super::prime::is_prime(&p, 0) {
            break p;
        }
    };
    let e = (&p - BigUint::one()) / &q;
    let mut h = BigUint::from(2u32);
    let g = loop {
        let g = h.modpow(&e, &p);
        if g > BigUint::one() {
            break g;
        }
        h += BigUint::one();
    };
    let components = dsa::Components::from_components(p, q, g).map_err(|_| failed())?;
    Ok(dsa::SigningKey::generate(&mut rng(), components))
}

// ---------------------------------------------------------------------------
// RSA primitives
// ---------------------------------------------------------------------------

fn rsa_private_op(k: &RsaPrivateKey, m: &[u8]) -> KResult<Vec<u8>> {
    let c = BigUint::from_bytes_be(m);
    let r = rsa::hazmat::rsa_decrypt_and_check(k, Some(&mut rng()), &c).map_err(|e| err("Error", &e.to_string(), None))?;
    Ok(left_pad(&r.to_bytes_be(), k.size()))
}

fn rsa_public_op(k: &RsaPublicKey, s: &[u8]) -> KResult<Vec<u8>> {
    let c = BigUint::from_bytes_be(s);
    if &c >= k.n() {
        return Err(err("Error", "error:02000084:rsa routines::data too large for modulus", Some("ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_MODULUS")));
    }
    let r = rsa::hazmat::rsa_encrypt(k, &c).map_err(|e| err("Error", &e.to_string(), None))?;
    Ok(left_pad(&r.to_bytes_be(), k.size()))
}

fn left_pad(b: &[u8], len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len.saturating_sub(b.len())];
    v.extend_from_slice(b);
    v
}

fn digest_info_prefix(digest: &str) -> Option<&'static [u8]> {
    Some(match digest {
        "md5" => &[0x30, 0x20, 0x30, 0x0c, 0x06, 0x08, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x05, 0x05, 0x00, 0x04, 0x10],
        "sha1" => &[0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14],
        "sha224" => &[0x30, 0x2d, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x04, 0x05, 0x00, 0x04, 0x1c],
        "sha256" => &[0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20],
        "sha384" => &[0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0x04, 0x30],
        "sha512" => &[0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0x04, 0x40],
        "sha512-224" => &[0x30, 0x2d, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x05, 0x05, 0x00, 0x04, 0x1c],
        "sha512-256" => &[0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x06, 0x05, 0x00, 0x04, 0x20],
        _ => return None,
    })
}

/// EMSA-PKCS1-v1_5 type 1 padding of `t` to `k` bytes.
fn pkcs1_type1(t: &[u8], k: usize) -> KResult<Vec<u8>> {
    if t.len() + 11 > k {
        return Err(err("Error", "error:0200006E:rsa routines::data too large for key size", Some("ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_KEY_SIZE")));
    }
    let mut em = vec![0x00, 0x01];
    em.extend(std::iter::repeat_n(0xff, k - t.len() - 3));
    em.push(0);
    em.extend_from_slice(t);
    Ok(em)
}

fn mgf1(digest: &str, seed: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut counter = 0u32;
    while out.len() < len {
        let mut input = seed.to_vec();
        input.extend_from_slice(&counter.to_be_bytes());
        out.extend(hash(digest, &input).unwrap_or_default());
        counter += 1;
    }
    out.truncate(len);
    out
}

/// RSA-PSS salt lengths (Node's `RSA_PSS_SALTLEN_*`).
const SALTLEN_DIGEST: i64 = -1;
const SALTLEN_MAX_SIGN: i64 = -2;
const SALTLEN_AUTO: i64 = -2;

fn pss_sign(k: &RsaPrivateKey, digest: &str, m_hash: &[u8], salt_len: i64) -> KResult<Vec<u8>> {
    let mod_bits = k.n().bits();
    let em_bits = mod_bits - 1;
    let em_len = em_bits.div_ceil(8);
    let h_len = m_hash.len();
    let max_salt = em_len.checked_sub(h_len + 2).ok_or_else(|| err("Error", "error:0200006E:rsa routines::data too large for key size", None))?;
    let s_len = match salt_len {
        SALTLEN_DIGEST => h_len,
        SALTLEN_MAX_SIGN => max_salt,
        n if n >= 0 => n as usize,
        _ => return Err(err("Error", "invalid salt length", None)),
    };
    if s_len > max_salt {
        return Err(err("Error", "error:0200006E:rsa routines::data too large for key size", Some("ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_KEY_SIZE")));
    }
    let mut salt = vec![0u8; s_len];
    rng().fill_bytes(&mut salt);
    let mut m_prime = vec![0u8; 8];
    m_prime.extend_from_slice(m_hash);
    m_prime.extend_from_slice(&salt);
    let h = hash(digest, &m_prime).unwrap_or_default();
    let db_len = em_len - h_len - 1;
    let mut db = vec![0u8; db_len - s_len - 1];
    db.push(1);
    db.extend_from_slice(&salt);
    let mask = mgf1(digest, &h, db_len);
    for (d, m) in db.iter_mut().zip(mask) {
        *d ^= m;
    }
    db[0] &= 0xff >> (8 * em_len - em_bits);
    let mut em = db;
    em.extend_from_slice(&h);
    em.push(0xbc);
    rsa_private_op(k, &left_pad(&em, k.size()))
}

fn pss_verify(k: &RsaPublicKey, digest: &str, m_hash: &[u8], sig: &[u8], salt_len: i64) -> bool {
    if sig.len() != k.size() {
        return false;
    }
    let Ok(m) = rsa_public_op(k, sig) else { return false };
    let em_bits = k.n().bits() - 1;
    let em_len = em_bits.div_ceil(8);
    let em = &m[m.len() - em_len..];
    if m.len() > em_len && m[0] != 0 {
        return false;
    }
    let h_len = m_hash.len();
    if em_len < h_len + 2 || em[em_len - 1] != 0xbc {
        return false;
    }
    let db_len = em_len - h_len - 1;
    let (masked, h) = (&em[..db_len], &em[db_len..em_len - 1]);
    let top = 0xffu8 >> (8 * em_len - em_bits);
    if masked[0] & !top != 0 {
        return false;
    }
    let mask = mgf1(digest, h, db_len);
    let mut db: Vec<u8> = masked.iter().zip(mask).map(|(a, b)| a ^ b).collect();
    db[0] &= top;
    let Some(sep) = db.iter().position(|&b| b != 0) else { return false };
    if db[sep] != 1 {
        return false;
    }
    let salt = &db[sep + 1..];
    match salt_len {
        SALTLEN_DIGEST if salt.len() != h_len => return false,
        n if n >= 0 && salt.len() != n as usize => return false,
        _ => {}
    }
    let mut m_prime = vec![0u8; 8];
    m_prime.extend_from_slice(m_hash);
    m_prime.extend_from_slice(salt);
    hash(digest, &m_prime).is_some_and(|h2| h2 == h)
}

// ---------------------------------------------------------------------------
// Sign / verify
// ---------------------------------------------------------------------------

/// Node's `RSA_PKCS1_PSS_PADDING`.
const RSA_PKCS1_PSS_PADDING: i64 = 6;

pub(crate) struct SignOpts {
    pub padding: Option<i64>,
    pub salt_len: Option<i64>,
    /// `ieee-p1363` (raw r||s) instead of DER for ECDSA.
    pub p1363: bool,
}

fn invalid_digest() -> CipherError {
    err("TypeError", "Invalid digest", Some("ERR_CRYPTO_INVALID_DIGEST"))
}

fn resolve_digest(digest: Option<&str>) -> KResult<&'static str> {
    match digest {
        None => Ok("sha256"),
        Some(d) => digest_name(d).ok_or_else(invalid_digest),
    }
}

pub(crate) fn sign(digest: Option<&str>, key: &PrivKey, data: &[u8], opts: &SignOpts) -> KResult<Vec<u8>> {
    match key {
        PrivKey::Rsa(k, pss_key) => {
            let d = resolve_digest(digest)?;
            let h = hash(d, data).ok_or_else(invalid_digest)?;
            if *pss_key || opts.padding == Some(RSA_PKCS1_PSS_PADDING) {
                return pss_sign(k, d, &h, opts.salt_len.unwrap_or(SALTLEN_MAX_SIGN));
            }
            let mut t = digest_info_prefix(d).ok_or_else(invalid_digest)?.to_vec();
            t.extend_from_slice(&h);
            rsa_private_op(k, &pkcs1_type1(&t, k.size())?)
        }
        PrivKey::Ec(e) => {
            let d = resolve_digest(digest)?;
            let h = hash(d, data).ok_or_else(invalid_digest)?;
            ec_sign_prehash(e, &h, opts.p1363)
        }
        PrivKey::Ed25519(seed) => {
            use ed25519_dalek::Signer;
            eddsa_digest(digest)?;
            Ok(ed25519_dalek::SigningKey::from_bytes(seed).sign(data).to_bytes().to_vec())
        }
        PrivKey::Ed448(seed) => {
            eddsa_digest(digest)?;
            Ok(super::ed448::ed448_sign(seed, data).to_vec())
        }
        PrivKey::Dsa(k) => {
            use dsa::signature::SignatureEncoding;
            use dsa::signature::hazmat::RandomizedPrehashSigner;
            let d = resolve_digest(digest)?;
            let h = hash(d, data).ok_or_else(invalid_digest)?;
            let sig = k.sign_prehash_with_rng(&mut rng(), &h).map_err(|e| err("Error", &e.to_string(), None))?;
            if opts.p1363 {
                let n = k.verifying_key().components().q().bits().div_ceil(8);
                let mut out = left_pad(&sig.r().to_bytes_be(), n);
                out.extend(left_pad(&sig.s().to_bytes_be(), n));
                return Ok(out);
            }
            Ok(sig.to_vec())
        }
        PrivKey::X25519(_) | PrivKey::X448(_) => Err(not_for_keytype()),
    }
}

fn not_for_keytype() -> CipherError {
    err("Error", "error:03000096:digital envelope routines::operation not supported for this keytype", Some("ERR_OSSL_EVP_OPERATION_NOT_SUPPORTED_FOR_THIS_KEYTYPE"))
}

/// EdDSA takes no digest (OpenSSL: "invalid digest").
fn eddsa_digest(digest: Option<&str>) -> KResult<()> {
    match digest {
        None => Ok(()),
        Some(_) => Err(err("Error", "error:1C80007A:Provider routines::invalid digest", Some("ERR_OSSL_INVALID_DIGEST"))),
    }
}

/// Digests shorter than half the field size (SHA-256 with P-521) are rejected by the ecdsa
/// crate; left-padding keeps the same integer value, which is what OpenSSL signs.
fn widen_prehash(curve: Curve, h: &[u8]) -> Vec<u8> {
    if h.len() >= curve.field_len() / 2 { h.to_vec() } else { left_pad(h, curve.field_len()) }
}

pub(crate) fn ec_sign_prehash(e: &EcSecret, h: &[u8], p1363: bool) -> KResult<Vec<u8>> {
    let h = &widen_prehash(e.curve(), h);
    let fail = |e: p256::ecdsa::Error| err("Error", &e.to_string(), None);
    ec_dispatch!(e, EcSecret, k, m => {
        use m::ecdsa::signature::hazmat::PrehashSigner;
        let sk = m::ecdsa::SigningKey::from_bytes(&k.to_bytes()).map_err(fail)?;
        let sig: m::ecdsa::Signature = sk.sign_prehash(h).map_err(fail)?;
        Ok(if p1363 { sig.to_bytes().to_vec() } else { sig.to_der().as_bytes().to_vec() })
    })
}

pub(crate) fn verify(digest: Option<&str>, key: &PubKey, data: &[u8], sig: &[u8], opts: &SignOpts) -> KResult<bool> {
    match key {
        PubKey::Rsa(k, pss_key) => {
            let d = resolve_digest(digest)?;
            let h = hash(d, data).ok_or_else(invalid_digest)?;
            if *pss_key || opts.padding == Some(RSA_PKCS1_PSS_PADDING) {
                return Ok(pss_verify(k, d, &h, sig, opts.salt_len.unwrap_or(SALTLEN_AUTO)));
            }
            if sig.len() != k.size() {
                return Ok(false);
            }
            let Ok(em) = rsa_public_op(k, sig) else { return Ok(false) };
            let mut t = digest_info_prefix(d).ok_or_else(invalid_digest)?.to_vec();
            t.extend_from_slice(&h);
            Ok(pkcs1_type1(&t, k.size()).is_ok_and(|expected| expected == em))
        }
        PubKey::Ec(e) => {
            let d = resolve_digest(digest)?;
            let h = hash(d, data).ok_or_else(invalid_digest)?;
            Ok(ec_verify_prehash(e, &h, sig, opts.p1363))
        }
        PubKey::Ed25519(p) => {
            use ed25519_dalek::Verifier;
            eddsa_digest(digest)?;
            let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(p) else { return Ok(false) };
            let Ok(sig) = <[u8; 64]>::try_from(sig) else { return Ok(false) };
            Ok(vk.verify(data, &ed25519_dalek::Signature::from_bytes(&sig)).is_ok())
        }
        PubKey::Ed448(p) => {
            eddsa_digest(digest)?;
            Ok(super::ed448::ed448_verify(p, data, sig))
        }
        PubKey::Dsa(k) => {
            use dsa::signature::hazmat::PrehashVerifier;
            let d = resolve_digest(digest)?;
            let h = hash(d, data).ok_or_else(invalid_digest)?;
            let parsed = if opts.p1363 {
                if sig.is_empty() || !sig.len().is_multiple_of(2) {
                    return Ok(false);
                }
                let (r, s) = sig.split_at(sig.len() / 2);
                dsa::Signature::from_components(BigUint::from_bytes_be(r), BigUint::from_bytes_be(s))
            } else {
                dsa::Signature::try_from(sig)
            };
            let Ok(sig) = parsed else { return Ok(false) };
            Ok(k.verify_prehash(&h, &sig).is_ok())
        }
        PubKey::X25519(_) | PubKey::X448(_) => Err(not_for_keytype()),
    }
}

pub(crate) fn ec_verify_prehash(e: &EcPublic, h: &[u8], sig: &[u8], p1363: bool) -> bool {
    let h = &widen_prehash(e.curve(), h);
    let point = e.to_sec1(false);
    curve_dispatch!(e.curve(), m => {
        use m::ecdsa::signature::hazmat::PrehashVerifier;
        let Ok(vk) = m::ecdsa::VerifyingKey::from_sec1_bytes(&point) else { return false };
        let parsed = if p1363 { m::ecdsa::Signature::from_slice(sig) } else { m::ecdsa::Signature::from_der(sig) };
        let Ok(sig) = parsed else { return false };
        // OpenSSL accepts high-S signatures; k256 only verifies normalized ones.
        let sig = sig.normalize_s().unwrap_or(sig);
        vk.verify_prehash(h, &sig).is_ok()
    })
}

// ---------------------------------------------------------------------------
// Key agreement
// ---------------------------------------------------------------------------

pub(crate) fn ecdh(curve: Curve, private: &[u8], public: &[u8]) -> KResult<Vec<u8>> {
    let secret = EcSecret::from_bytes(curve, private)?;
    let peer = EcPublic::from_sec1(curve, public)?;
    ecdh_keys(&secret, &peer)
}

fn ecdh_keys(secret: &EcSecret, peer: &EcPublic) -> KResult<Vec<u8>> {
    let mismatch = || err("Error", "error:1C8000A5:Provider routines::different curves", None);
    Ok(match (secret, peer) {
        (EcSecret::P256(s), EcPublic::P256(p)) => p256::ecdh::diffie_hellman(s.to_nonzero_scalar(), p.as_affine()).raw_secret_bytes().to_vec(),
        (EcSecret::P384(s), EcPublic::P384(p)) => p384::ecdh::diffie_hellman(s.to_nonzero_scalar(), p.as_affine()).raw_secret_bytes().to_vec(),
        (EcSecret::P521(s), EcPublic::P521(p)) => p521::ecdh::diffie_hellman(s.to_nonzero_scalar(), p.as_affine()).raw_secret_bytes().to_vec(),
        (EcSecret::K256(s), EcPublic::K256(p)) => k256::ecdh::diffie_hellman(s.to_nonzero_scalar(), p.as_affine()).raw_secret_bytes().to_vec(),
        _ => return Err(mismatch()),
    })
}

/// `crypto.diffieHellman({ privateKey, publicKey })` / WebCrypto ECDH and X25519.
pub(crate) fn derive(private: &PrivKey, public: &PubKey) -> KResult<Vec<u8>> {
    match (private, public) {
        (PrivKey::Ec(s), PubKey::Ec(p)) => ecdh_keys(s, p),
        (PrivKey::X25519(s), PubKey::X25519(p)) => {
            let shared = x25519_dalek::StaticSecret::from(*s).diffie_hellman(&x25519_dalek::PublicKey::from(*p));
            if !shared.was_contributory() {
                return Err(err("Error", "error:1C8000A5:Provider routines::failed during derivation", None));
            }
            Ok(shared.to_bytes().to_vec())
        }
        (PrivKey::X448(s), PubKey::X448(p)) => super::ed448::x448(s, p)
            .map(|b| b.to_vec())
            .ok_or_else(|| err("Error", "error:1C8000A5:Provider routines::failed during derivation", None)),
        _ => Err(err(
            "Error",
            &format!("Incompatible key types for Diffie-Hellman: {} and {}", private.asym(), public.asym()),
            Some("ERR_CRYPTO_INCOMPATIBLE_KEY"),
        )),
    }
}

/// Finite-field Diffie-Hellman: `base^exp mod modulus`, left-padded to the modulus length.
pub(crate) fn mod_pow(base: &[u8], exp: &[u8], modulus: &[u8]) -> Vec<u8> {
    let m = BigUint::from_bytes_be(modulus);
    let r = BigUint::from_bytes_be(base).modpow(&BigUint::from_bytes_be(exp), &m);
    left_pad(&r.to_bytes_be(), modulus.len())
}

/// RFC 2409 / RFC 3526 MODP groups for `crypto.getDiffieHellman(name)` (generator 2).
pub(crate) fn modp_group(name: &str) -> Option<&'static str> {
    Some(match name {
        "modp1" => "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a63a3620ffffffffffffffff",
        "modp2" => "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7edee386bfb5a899fa5ae9f24117c4b1fe649286651ece65381ffffffffffffffff",
        "modp5" => "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7edee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf0598da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb9ed529077096966d670c354e4abc9804f1746c08ca237327ffffffffffffffff",
        "modp14" => "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7edee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf0598da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3be39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf6955817183995497cea956ae515d2261898fa051015728e5a8aacaa68ffffffffffffffff",
        "modp15" => "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7edee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf0598da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3be39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf6955817183995497cea956ae515d2261898fa051015728e5a8aaac42dad33170d04507a33a85521abdf1cba64ecfb850458dbef0a8aea71575d060c7db3970f85a6e1e4c7abf5ae8cdb0933d71e8c94e04a25619dcee3d2261ad2ee6bf12ffa06d98a0864d87602733ec86a64521f2b18177b200cbbe117577a615d6c770988c0bad946e208e24fa074e5ab3143db5bfce0fd108e4b82d120a93ad2caffffffffffffffff",
        "modp16" => "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7edee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf0598da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3be39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf6955817183995497cea956ae515d2261898fa051015728e5a8aaac42dad33170d04507a33a85521abdf1cba64ecfb850458dbef0a8aea71575d060c7db3970f85a6e1e4c7abf5ae8cdb0933d71e8c94e04a25619dcee3d2261ad2ee6bf12ffa06d98a0864d87602733ec86a64521f2b18177b200cbbe117577a615d6c770988c0bad946e208e24fa074e5ab3143db5bfce0fd108e4b82d120a92108011a723c12a787e6d788719a10bdba5b2699c327186af4e23c1a946834b6150bda2583e9ca2ad44ce8dbbbc2db04de8ef92e8efc141fbecaa6287c59474e6bc05d99b2964fa090c3a2233ba186515be7ed1f612970cee2d7afb81bdd762170481cd0069127d5b05aa993b4ea988d8fddc186ffb7dc90a6c08f4df435c934063199ffffffffffffffff",
        "modp17" => "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7edee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf0598da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3be39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf6955817183995497cea956ae515d2261898fa051015728e5a8aaac42dad33170d04507a33a85521abdf1cba64ecfb850458dbef0a8aea71575d060c7db3970f85a6e1e4c7abf5ae8cdb0933d71e8c94e04a25619dcee3d2261ad2ee6bf12ffa06d98a0864d87602733ec86a64521f2b18177b200cbbe117577a615d6c770988c0bad946e208e24fa074e5ab3143db5bfce0fd108e4b82d120a92108011a723c12a787e6d788719a10bdba5b2699c327186af4e23c1a946834b6150bda2583e9ca2ad44ce8dbbbc2db04de8ef92e8efc141fbecaa6287c59474e6bc05d99b2964fa090c3a2233ba186515be7ed1f612970cee2d7afb81bdd762170481cd0069127d5b05aa993b4ea988d8fddc186ffb7dc90a6c08f4df435c93402849236c3fab4d27c7026c1d4dcb2602646dec9751e763dba37bdf8ff9406ad9e530ee5db382f413001aeb06a53ed9027d831179727b0865a8918da3edbebcf9b14ed44ce6cbaced4bb1bdb7f1447e6cc254b332051512bd7af426fb8f401378cd2bf5983ca01c64b92ecf032ea15d1721d03f482d7ce6e74fef6d55e702f46980c82b5a84031900b1c9e59e7c97fbec7e8f323a97a7e36cc88be0f1d45b7ff585ac54bd407b22b4154aacc8f6d7ebf48e1d814cc5ed20f8037e0a79715eef29be32806a1d58bb7c5da76f550aa3d8a1fbff0eb19ccb1a313d55cda56c9ec2ef29632387fe8d76e3c0468043e8f663f4860ee12bf2d5b0b7474d6e694f91e6dcc4024ffffffffffffffff",
        "modp18" => "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7edee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf0598da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3be39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf6955817183995497cea956ae515d2261898fa051015728e5a8aaac42dad33170d04507a33a85521abdf1cba64ecfb850458dbef0a8aea71575d060c7db3970f85a6e1e4c7abf5ae8cdb0933d71e8c94e04a25619dcee3d2261ad2ee6bf12ffa06d98a0864d87602733ec86a64521f2b18177b200cbbe117577a615d6c770988c0bad946e208e24fa074e5ab3143db5bfce0fd108e4b82d120a92108011a723c12a787e6d788719a10bdba5b2699c327186af4e23c1a946834b6150bda2583e9ca2ad44ce8dbbbc2db04de8ef92e8efc141fbecaa6287c59474e6bc05d99b2964fa090c3a2233ba186515be7ed1f612970cee2d7afb81bdd762170481cd0069127d5b05aa993b4ea988d8fddc186ffb7dc90a6c08f4df435c93402849236c3fab4d27c7026c1d4dcb2602646dec9751e763dba37bdf8ff9406ad9e530ee5db382f413001aeb06a53ed9027d831179727b0865a8918da3edbebcf9b14ed44ce6cbaced4bb1bdb7f1447e6cc254b332051512bd7af426fb8f401378cd2bf5983ca01c64b92ecf032ea15d1721d03f482d7ce6e74fef6d55e702f46980c82b5a84031900b1c9e59e7c97fbec7e8f323a97a7e36cc88be0f1d45b7ff585ac54bd407b22b4154aacc8f6d7ebf48e1d814cc5ed20f8037e0a79715eef29be32806a1d58bb7c5da76f550aa3d8a1fbff0eb19ccb1a313d55cda56c9ec2ef29632387fe8d76e3c0468043e8f663f4860ee12bf2d5b0b7474d6e694f91e6dbe115974a3926f12fee5e438777cb6a932df8cd8bec4d073b931ba3bc832b68d9dd300741fa7bf8afc47ed2576f6936ba424663aab639c5ae4f5683423b4742bf1c978238f16cbe39d652de3fdb8befc848ad922222e04a4037c0713eb57a81a23f0c73473fc646cea306b4bcbc8862f8385ddfa9d4b7fa2c087e879683303ed5bdd3a062b3cf5b3a278a66d2a13f83f44f82ddf310ee074ab6a364597e899a0255dc164f31cc50846851df9ab48195ded7ea1b1d510bd7ee74d73faf36bc31ecfa268359046f4eb879f924009438b481c6cd7889a002ed5ee382bc9190da6fc026e479558e4475677e9aa9e3050e2765694dfc81f56e880b96e7160c980dd98edd3dfffffffffffffffff",
        _ => return None,
    })
}

/// `ops.dhGroup(name) -> prime bytes | undefined`
fn op_dh_group(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_string(args, 0, ctx)?;
    let Some(hex) = modp_group(&name) else { return Ok(JsValue::undefined()) };
    let bytes: Vec<u8> = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0)).collect();
    bytes_to_js(&bytes, ctx)
}

// ---------------------------------------------------------------------------
// RSA encryption
// ---------------------------------------------------------------------------

const RSA_NO_PADDING: i64 = 3;
const RSA_PKCS1_PADDING: i64 = 1;
const RSA_PKCS1_OAEP_PADDING: i64 = 4;

fn oaep(digest: &str, label: Option<&[u8]>) -> KResult<rsa::Oaep> {
    let label = label.filter(|l| !l.is_empty()).map(|l| String::from_utf8_lossy(l).into_owned());
    macro_rules! mk {
        ($d:ty) => {
            match label {
                Some(l) => rsa::Oaep::new_with_label::<$d, _>(l),
                None => rsa::Oaep::new::<$d>(),
            }
        };
    }
    Ok(match digest {
        "sha1" => mk!(sha1::Sha1),
        "sha224" => mk!(sha2::Sha224),
        "sha256" => mk!(sha2::Sha256),
        "sha384" => mk!(sha2::Sha384),
        "sha512" => mk!(sha2::Sha512),
        "md5" => mk!(md5::Md5),
        _ => return Err(invalid_digest()),
    })
}

/// `publicEncrypt`/`privateDecrypt`/`privateEncrypt`/`publicDecrypt`.
pub(crate) fn rsa_crypt(op: &str, key: &Imported, data: &[u8], padding: i64, oaep_hash: &str, label: Option<&[u8]>) -> KResult<Vec<u8>> {
    let not_rsa = || err("Error", "error:1C8000A5:Provider routines::operation not supported for this keytype", Some("ERR_OSSL_OPERATION_NOT_SUPPORTED_FOR_THIS_KEYTYPE"));
    let public = match key {
        Imported::Private(PrivKey::Rsa(k, _)) => Box::new(k.to_public_key()),
        Imported::Public(PubKey::Rsa(k, _)) => k.clone(),
        _ => return Err(not_rsa()),
    };
    let private = match key {
        Imported::Private(PrivKey::Rsa(k, _)) => Some(k),
        _ => None,
    };
    let oaep_err = || err("Error", "error:02000079:rsa routines::oaep decoding error", Some("ERR_OSSL_RSA_OAEP_DECODING_ERROR"));
    let digest = digest_name(oaep_hash).ok_or_else(invalid_digest)?;
    match op {
        "publicEncrypt" => match padding {
            RSA_PKCS1_OAEP_PADDING => public.encrypt(&mut rng(), oaep(digest, label)?, data).map_err(|e| err("Error", &e.to_string(), None)),
            RSA_PKCS1_PADDING => public.encrypt(&mut rng(), rsa::Pkcs1v15Encrypt, data).map_err(|e| err("Error", &e.to_string(), None)),
            RSA_NO_PADDING => rsa_public_op(&public, data),
            _ => Err(err("Error", "error:1C8000A5:Provider routines::illegal or unsupported padding mode", None)),
        },
        "privateDecrypt" => {
            let k = private.ok_or_else(not_rsa)?;
            match padding {
                RSA_PKCS1_OAEP_PADDING => k.decrypt(oaep(digest, label)?, data).map_err(|_| oaep_err()),
                RSA_PKCS1_PADDING => k.decrypt(rsa::Pkcs1v15Encrypt, data).map_err(|_| err("Error", "error:02000093:rsa routines::pkcs decoding error", None)),
                RSA_NO_PADDING => rsa_private_op(k, data),
                _ => Err(err("Error", "error:1C8000A5:Provider routines::illegal or unsupported padding mode", None)),
            }
        }
        "privateEncrypt" => {
            let k = private.ok_or_else(not_rsa)?;
            match padding {
                RSA_NO_PADDING => rsa_private_op(k, data),
                _ => rsa_private_op(k, &pkcs1_type1(data, k.size())?),
            }
        }
        _ => {
            // publicDecrypt: undo type-1 padding.
            let em = rsa_public_op(&public, data)?;
            if padding == RSA_NO_PADDING {
                return Ok(em);
            }
            let bad = || err("Error", "error:02000072:rsa routines::invalid padding", None);
            if em.len() < 11 || em[0] != 0 || em[1] != 1 {
                return Err(bad());
            }
            let sep = em[2..].iter().position(|&b| b != 0xff).map(|i| i + 2).ok_or_else(bad)?;
            if em[sep] != 0 || sep < 10 {
                return Err(bad());
            }
            Ok(em[sep + 1..].to_vec())
        }
    }
}

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

fn get_str(ctx: &mut Context, obj: &JsValue, key: &str) -> JsResult<Option<String>> {
    let Some(o) = obj.as_object() else { return Ok(None) };
    let v = o.get(js_str(key).as_string().unwrap(), ctx)?;
    if v.is_null_or_undefined() { Ok(None) } else { Ok(Some(v.to_string(ctx)?.to_std_string_lossy())) }
}

fn get_num(ctx: &mut Context, obj: &JsValue, key: &str) -> JsResult<Option<f64>> {
    let Some(o) = obj.as_object() else { return Ok(None) };
    let v = o.get(js_str(key).as_string().unwrap(), ctx)?;
    if v.is_null_or_undefined() { Ok(None) } else { Ok(Some(v.to_number(ctx)?)) }
}

fn get_bytes(ctx: &mut Context, obj: &JsValue, key: &str) -> JsResult<Option<Vec<u8>>> {
    let Some(o) = obj.as_object() else { return Ok(None) };
    let v = o.get(js_str(key).as_string().unwrap(), ctx)?;
    if v.is_null_or_undefined() { Ok(None) } else { Ok(Some(bytes_arg(&v, ctx)?)) }
}

/// Describe a key for JS: `{ type, asym, der, details }`.
fn describe(ctx: &mut Context, key: &Imported) -> JsResult<JsValue> {
    let (kind, asym, der) = match key {
        Imported::Private(k) => ("private", k.asym(), k.to_pkcs8()),
        Imported::Public(k) => ("public", k.asym(), k.to_spki()),
    };
    let der = bytes_to_js(&der, ctx)?;
    let details = details(ctx, key)?;
    object(ctx, vec![("type", js_str(kind)), ("asym", js_str(asym)), ("der", der), ("details", details)])
}

fn details(ctx: &mut Context, key: &Imported) -> JsResult<JsValue> {
    let rsa = match key {
        Imported::Private(PrivKey::Rsa(k, _)) => Some((k.n().bits(), k.e().to_str_radix(16))),
        Imported::Public(PubKey::Rsa(k, _)) => Some((k.n().bits(), k.e().to_str_radix(16))),
        _ => None,
    };
    if let Some((bits, e)) = rsa {
        return object(ctx, vec![("modulusLength", JsValue::from(bits as u32)), ("publicExponentHex", js_str(&e))]);
    }
    let dsa = match key {
        Imported::Private(PrivKey::Dsa(k)) => Some(k.verifying_key().components().clone()),
        Imported::Public(PubKey::Dsa(k)) => Some(k.components().clone()),
        _ => None,
    };
    if let Some(c) = dsa {
        return object(ctx, vec![("modulusLength", JsValue::from(c.p().bits() as u32)), ("divisorLength", JsValue::from(c.q().bits() as u32))]);
    }
    let curve = match key {
        Imported::Private(PrivKey::Ec(e)) => Some(e.curve()),
        Imported::Public(PubKey::Ec(e)) => Some(e.curve()),
        _ => None,
    };
    match curve {
        Some(c) => object(ctx, vec![("namedCurve", js_str(c.openssl_name()))]),
        None => object(ctx, vec![]),
    }
}

fn throw(ctx: &mut Context, e: CipherError) -> crate::js::JsError {
    e.throw(ctx)
}

/// Parse a key described by JS: `{ type: "private"|"public", der }` (from a KeyObject).
fn key_from_js(ctx: &mut Context, v: &JsValue) -> JsResult<Imported> {
    let kind = get_str(ctx, v, "type")?.unwrap_or_default();
    let der = get_bytes(ctx, v, "der")?.unwrap_or_default();
    let r = if kind == "private" { PrivKey::from_pkcs8(&der).map(Imported::Private) } else { PubKey::from_spki(&der).map(Imported::Public) };
    r.map_err(|e| throw(ctx, e))
}

/// `ops.keyImport(data, { format, type, passphrase, private }) -> description`
fn op_key_import(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let data = bytes_arg(args.get_or_undefined(0), ctx)?;
    let opts = args.get_or_undefined(1).clone();
    let format = get_str(ctx, &opts, "format")?.unwrap_or_else(|| "pem".into());
    let typ = get_str(ctx, &opts, "type")?.unwrap_or_default();
    let pass = get_bytes(ctx, &opts, "passphrase")?;
    let want_private = opts.as_object().map(|o| o.get(js_string!("private"), ctx)).transpose()?.is_some_and(|v| v.to_boolean());
    match import(&data, &format, &typ, pass.as_deref(), want_private) {
        Ok(k) => describe(ctx, &k),
        Err(e) => Err(throw(ctx, e)),
    }
}

/// `ops.keyImportJwk(json, private) -> description`
fn op_key_import_jwk(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let json = arg_string(args, 0, ctx)?;
    let want_private = args.get_or_undefined(1).to_boolean();
    let jwk: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
    match import_jwk(&jwk, want_private) {
        Ok(k) => describe(ctx, &k),
        Err(e) => Err(throw(ctx, e)),
    }
}

/// `ops.keyImportRaw(asym, curve, bytes) -> description` (public keys)
fn op_key_import_raw(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let asym = arg_string(args, 0, ctx)?;
    let curve = crate::engine::arg_opt_string(args, 1, ctx)?;
    let raw = bytes_arg(args.get_or_undefined(2), ctx)?;
    match import_raw_public(&asym, curve.as_deref(), &raw) {
        Ok(k) => describe(ctx, &Imported::Public(k)),
        Err(e) => Err(throw(ctx, e)),
    }
}

/// `ops.keyExport(key, { format, type, cipher, passphrase }) -> string | Uint8Array | jwk JSON`
fn op_key_export(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let key = key_from_js(ctx, args.get_or_undefined(0))?;
    let opts = args.get_or_undefined(1).clone();
    let format = get_str(ctx, &opts, "format")?.unwrap_or_else(|| "pem".into());
    let typ = get_str(ctx, &opts, "type")?.unwrap_or_default();
    let cipher = get_str(ctx, &opts, "cipher")?;
    let pass = get_bytes(ctx, &opts, "passphrase")?;
    let r = match &key {
        Imported::Private(k) => export_private(k, &format, &typ, cipher.as_deref(), pass.as_deref()),
        Imported::Public(k) => export_public(k, &format, &typ),
    };
    match r {
        Ok(Exported::Text(s)) => Ok(js_str(&s)),
        Ok(Exported::Bytes(b)) => bytes_to_js(&b, ctx),
        Ok(Exported::Json(j)) => object(ctx, vec![("jwk", js_str(&j.to_string()))]),
        Err(e) => Err(throw(ctx, e)),
    }
}

/// `ops.keyRaw(key) -> bytes`: raw public bytes (EC point, OKP key) or private scalar/seed.
fn op_key_raw(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let key = key_from_js(ctx, args.get_or_undefined(0))?;
    let compressed = args.get_or_undefined(1).to_boolean();
    let raw = match &key {
        Imported::Public(PubKey::Ec(e)) => e.to_sec1(compressed),
        Imported::Public(PubKey::Ed25519(p) | PubKey::X25519(p)) => p.to_vec(),
        Imported::Public(PubKey::Ed448(p)) => p.to_vec(),
        Imported::Public(PubKey::X448(p)) => p.to_vec(),
        Imported::Private(PrivKey::Ec(e)) => e.to_bytes(),
        Imported::Private(PrivKey::Ed25519(s) | PrivKey::X25519(s)) => s.to_vec(),
        Imported::Private(PrivKey::Ed448(s)) => s.to_vec(),
        Imported::Private(PrivKey::X448(s)) => s.to_vec(),
        _ => return Err(throw(ctx, unsupported())),
    };
    bytes_to_js(&raw, ctx)
}

/// `ops.keyPublic(privateKey) -> description` of the public half.
fn op_key_public(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    match key_from_js(ctx, args.get_or_undefined(0))? {
        Imported::Private(k) => describe(ctx, &Imported::Public(k.public())),
        public => describe(ctx, &public),
    }
}

type GenOpts = (String, usize, u64, Option<String>, Option<usize>);

fn generate_from_opts(ctx: &mut Context, args: &[JsValue]) -> JsResult<GenOpts> {
    let asym = arg_string(args, 0, ctx)?;
    let opts = args.get_or_undefined(1).clone();
    let bits = get_num(ctx, &opts, "modulusLength")?.unwrap_or(2048.0) as usize;
    let exp = get_num(ctx, &opts, "publicExponent")?.unwrap_or(65537.0) as u64;
    let curve = get_str(ctx, &opts, "namedCurve")?;
    let divisor = get_num(ctx, &opts, "divisorLength")?.map(|n| n as usize);
    Ok((asym, bits, exp, curve, divisor))
}

/// `ops.keyGenerate(asym, opts, async) -> [private description, public description]`
fn op_key_generate(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (asym, bits, exp, curve, divisor) = generate_from_opts(ctx, args)?;
    let finish = |k: PrivKey, ctx: &mut Context| -> JsResult<JsValue> {
        let public = Imported::Public(k.public());
        let a = describe(ctx, &Imported::Private(k))?;
        let b = describe(ctx, &public)?;
        Ok(super::array(ctx, vec![a, b]))
    };
    if !args.get_or_undefined(2).to_boolean() {
        return match generate(&asym, bits, exp, curve.as_deref(), divisor) {
            Ok(k) => finish(k, ctx),
            Err(e) => Err(throw(ctx, e)),
        };
    }
    // Validate synchronously (Node throws argument errors from generateKeyPair itself).
    if asym == "ec" && Curve::from_name(curve.as_deref().unwrap_or_default()).is_none() {
        return Err(throw(ctx, err("TypeError", "Invalid EC curve name", Some("ERR_CRYPTO_INVALID_CURVE"))));
    }
    Ok(crate::engine::spawn_result(
        ctx,
        async move {
            match tokio::task::spawn_blocking(move || generate(&asym, bits, exp, curve.as_deref(), divisor)).await {
                Ok(r) => r.map_err(super::crypto::to_error),
                Err(e) => Err(crate::Error::Host(e.to_string())),
            }
        },
        finish,
    ))
}

fn sign_opts(ctx: &mut Context, v: &JsValue) -> JsResult<SignOpts> {
    Ok(SignOpts {
        padding: get_num(ctx, v, "padding")?.map(|n| n as i64),
        salt_len: get_num(ctx, v, "saltLength")?.map(|n| n as i64),
        p1363: get_str(ctx, v, "dsaEncoding")?.is_some_and(|s| s == "ieee-p1363"),
    })
}

/// `ops.sign(digest|null, key, data, opts) -> signature`
fn op_sign(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let digest = crate::engine::arg_opt_string(args, 0, ctx)?;
    let key = key_from_js(ctx, args.get_or_undefined(1))?;
    let data = bytes_arg(args.get_or_undefined(2), ctx)?;
    let opts = sign_opts(ctx, args.get_or_undefined(3))?;
    let Imported::Private(k) = key else {
        return Err(throw(ctx, err("TypeError", "Invalid key object type public, expected private.", Some("ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE"))));
    };
    match sign(digest.as_deref(), &k, &data, &opts) {
        Ok(s) => bytes_to_js(&s, ctx),
        Err(e) => Err(throw(ctx, e)),
    }
}

/// `ops.verify(digest|null, key, data, signature, opts) -> boolean`
fn op_verify(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let digest = crate::engine::arg_opt_string(args, 0, ctx)?;
    let key = key_from_js(ctx, args.get_or_undefined(1))?;
    let data = bytes_arg(args.get_or_undefined(2), ctx)?;
    let sig = bytes_arg(args.get_or_undefined(3), ctx)?;
    let opts = sign_opts(ctx, args.get_or_undefined(4))?;
    let public = match key {
        Imported::Private(k) => k.public(),
        Imported::Public(k) => k,
    };
    match verify(digest.as_deref(), &public, &data, &sig, &opts) {
        Ok(ok) => Ok(JsValue::from(ok)),
        Err(e) => Err(throw(ctx, e)),
    }
}

/// `ops.ecdh(op, curve, a, b, compressed)`: `generate` -> private scalar, `public`
/// (a = private) -> point, `compute` (a = private, b = peer point) -> secret, `convert`
/// (a = point) -> point.
fn op_ecdh(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let op = arg_string(args, 0, ctx)?;
    let curve_name = arg_string(args, 1, ctx)?;
    let Some(curve) = Curve::from_name(&curve_name) else {
        return Err(throw(ctx, err("TypeError", "Invalid EC curve name", Some("ERR_CRYPTO_INVALID_CURVE"))));
    };
    let a = bytes_arg(args.get_or_undefined(2), ctx)?;
    let b = bytes_arg(args.get_or_undefined(3), ctx)?;
    let compressed = args.get_or_undefined(4).to_boolean();
    let r = match op.as_str() {
        "generate" => Ok(EcSecret::generate(curve).to_bytes()),
        "public" => EcSecret::from_bytes(curve, &a).map(|s| s.public().to_sec1(compressed)),
        "compute" => ecdh(curve, &a, &b),
        _ => EcPublic::from_sec1(curve, &a).map(|p| p.to_sec1(compressed)),
    };
    match r {
        Ok(v) => bytes_to_js(&v, ctx),
        Err(e) => Err(throw(ctx, e)),
    }
}

/// `ops.deriveSecret(privateKey, publicKey) -> secret`
fn op_derive_secret(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let a = key_from_js(ctx, args.get_or_undefined(0))?;
    let b = key_from_js(ctx, args.get_or_undefined(1))?;
    let (Imported::Private(p), Imported::Public(q)) = (a, b) else {
        return Err(throw(ctx, err("TypeError", "Invalid key object type", Some("ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE"))));
    };
    match derive(&p, &q) {
        Ok(v) => bytes_to_js(&v, ctx),
        Err(e) => Err(throw(ctx, e)),
    }
}

/// `ops.modPow(base, exp, modulus) -> bytes`
fn op_mod_pow(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let base = bytes_arg(args.get_or_undefined(0), ctx)?;
    let exp = bytes_arg(args.get_or_undefined(1), ctx)?;
    let modulus = bytes_arg(args.get_or_undefined(2), ctx)?;
    bytes_to_js(&mod_pow(&base, &exp, &modulus), ctx)
}

/// `ops.rsaCrypt(op, key, data, padding, oaepHash, label)`
fn op_rsa_crypt(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let op = arg_string(args, 0, ctx)?;
    let key = key_from_js(ctx, args.get_or_undefined(1))?;
    let data = bytes_arg(args.get_or_undefined(2), ctx)?;
    let padding = args.get_or_undefined(3).to_number(ctx)? as i64;
    let hash = crate::engine::arg_opt_string(args, 4, ctx)?.unwrap_or_else(|| "sha1".into());
    let label = args.get_or_undefined(5);
    let label = if label.is_null_or_undefined() { None } else { Some(bytes_arg(label, ctx)?) };
    match rsa_crypt(&op, &key, &data, padding, &hash, label.as_deref()) {
        Ok(v) => bytes_to_js(&v, ctx),
        Err(e) => Err(throw(ctx, e)),
    }
}

/// `ops.certInfo(der) -> { serialHex, issuer, subject, validFrom, validTo, spki } | undefined`
fn op_cert_info(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let der = bytes_arg(args.get_or_undefined(0), ctx)?;
    let Some([serial, issuer, validity, subject, spki]) = cert_fields(&der) else { return Ok(JsValue::undefined()) };
    let serial_hex: String = serial.iter().skip_while(|&&b| b == 0).map(|b| format!("{b:02X}")).collect();
    let issuer = name_object(ctx, issuer)?;
    let subject = name_object(ctx, subject)?;
    let times: Vec<String> = read_tlv(validity).map(|(t, _)| children(t.content).iter().map(|c| String::from_utf8_lossy(c.content).into_owned()).collect()).unwrap_or_default();
    let spki = bytes_to_js(spki, ctx)?;
    let san = subject_alt_names(&der).map(|s| js_str(&s)).unwrap_or_default();
    object(
        ctx,
        vec![
            ("subjectaltname", san),
            ("serialNumber", js_str(&serial_hex)),
            ("issuer", issuer),
            ("subject", subject),
            ("validFromRaw", js_str(times.first().map(String::as_str).unwrap_or_default())),
            ("validToRaw", js_str(times.get(1).map(String::as_str).unwrap_or_default())),
            ("spki", spki),
        ],
    )
}

/// An X.501 Name as `{ CN: "...", O: "...", ... }`.
fn name_object(ctx: &mut Context, name: &[u8]) -> JsResult<JsValue> {
    let mut entries: Vec<(String, String)> = Vec::new();
    if let Some((seq, _)) = read_tlv(name) {
        for set in children(seq.content) {
            for atv in children(set.content) {
                let parts = children(atv.content);
                if parts.len() < 2 {
                    continue;
                }
                let oid = ObjectIdentifier::from_bytes(parts[0].content).map(|o| o.to_string()).unwrap_or_default();
                let key = match oid.as_str() {
                    "2.5.4.3" => "CN",
                    "2.5.4.6" => "C",
                    "2.5.4.7" => "L",
                    "2.5.4.8" => "ST",
                    "2.5.4.10" => "O",
                    "2.5.4.11" => "OU",
                    "1.2.840.113549.1.9.1" => "emailAddress",
                    other => other,
                };
                entries.push((key.to_string(), String::from_utf8_lossy(parts[1].content).into_owned()));
            }
        }
    }
    let obj = crate::js::JsObject::with_object_proto(ctx.intrinsics());
    for (k, v) in entries {
        obj.set(js_str(&k).as_string().unwrap(), js_str(&v), true, ctx)?;
    }
    Ok(obj.into())
}

/// `ops.pemDecode(text) -> [{ label, der }]`
fn op_pem_decode(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = arg_string(args, 0, ctx)?;
    let mut items = Vec::new();
    for pem in parse_pem_all(&text) {
        let der = bytes_to_js(&pem.der, ctx)?;
        items.push(object(ctx, vec![("label", js_str(&pem.label)), ("der", der)])?);
    }
    Ok(super::array(ctx, items))
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("keyImport", 2, op_key_import),
    ("keyImportJwk", 2, op_key_import_jwk),
    ("keyImportRaw", 3, op_key_import_raw),
    ("keyExport", 2, op_key_export),
    ("keyRaw", 2, op_key_raw),
    ("keyPublic", 1, op_key_public),
    ("keyGenerate", 3, op_key_generate),
    ("sign", 4, op_sign),
    ("verify", 5, op_verify),
    ("ecdh", 5, op_ecdh),
    ("deriveSecret", 2, op_derive_secret),
    ("modPow", 3, op_mod_pow),
    ("dhGroup", 1, op_dh_group),
    ("rsaCrypt", 6, op_rsa_crypt),
    ("certInfo", 1, op_cert_info),
    ("pemDecode", 1, op_pem_decode),
];

#[cfg(test)]
mod tests {
    use super::*;

    const ED_PEM: &str = "-----BEGIN PRIVATE KEY-----\r\nMC4CAQAwBQYDK2VwBCIEIHKj+sVa9WcD/q2DJUJaf43Kptc8xYuUQA4bOFj9vC8T\r\n-----END PRIVATE KEY-----";

    #[test]
    fn ed25519_pem_sign_verify() {
        let Imported::Private(k) = import(ED_PEM.as_bytes(), "pem", "", None, true).unwrap() else { panic!() };
        assert_eq!(k.asym(), "ed25519");
        let opts = SignOpts { padding: None, salt_len: None, p1363: false };
        let sig = sign(None, &k, b"a", &opts).unwrap();
        assert_eq!(sig.len(), 64);
        assert!(verify(None, &k.public(), b"a", &sig, &opts).unwrap());
        assert!(!verify(None, &k.public(), b"b", &sig, &opts).unwrap());
        // Round trip through PKCS#8 and SPKI.
        assert_eq!(PrivKey::from_pkcs8(&k.to_pkcs8()).unwrap().to_pkcs8(), k.to_pkcs8());
        assert_eq!(PubKey::from_spki(&k.public().to_spki()).unwrap().to_spki(), k.public().to_spki());
    }

    #[test]
    fn ec_keys_sign_verify_and_ecdh() {
        for curve in ["prime256v1", "secp384r1", "secp521r1"] {
            let a = generate("ec", 0, 0, Some(curve), None).unwrap();
            let b = generate("ec", 0, 0, Some(curve), None).unwrap();
            for p1363 in [false, true] {
                let opts = SignOpts { padding: None, salt_len: None, p1363 };
                let sig = sign(Some("sha256"), &a, b"msg", &opts).unwrap();
                assert!(verify(Some("SHA256"), &a.public(), b"msg", &sig, &opts).unwrap(), "{curve}");
                assert!(!verify(Some("sha256"), &b.public(), b"msg", &sig, &opts).unwrap(), "{curve}");
            }
            assert_eq!(derive(&a, &b.public()).unwrap(), derive(&b, &a.public()).unwrap());
            let pem = match export_private(&a, "pem", "sec1", None, None).unwrap() {
                Exported::Text(t) => t,
                _ => panic!(),
            };
            let Imported::Private(back) = import(pem.as_bytes(), "pem", "", None, true).unwrap() else { panic!() };
            assert_eq!(back.to_pkcs8(), a.to_pkcs8());
            let Exported::Json(jwk) = export_private(&a, "jwk", "", None, None).unwrap() else { panic!() };
            let Imported::Private(back) = import_jwk(&jwk, true).unwrap() else { panic!() };
            assert_eq!(back.to_pkcs8(), a.to_pkcs8());
        }
    }

    #[test]
    fn rsa_sign_verify_encrypt() {
        let k = generate("rsa", 1024, 65537, None, None).unwrap();
        let p = k.public();
        let plain = SignOpts { padding: None, salt_len: None, p1363: false };
        let sig = sign(Some("sha256"), &k, b"hello", &plain).unwrap();
        assert!(verify(Some("sha256"), &p, b"hello", &sig, &plain).unwrap());
        let PrivKey::Rsa(rk, _) = &k else { panic!() };
        // Interoperates with the rsa crate's PKCS#1 v1.5 verifier.
        let PubKey::Rsa(rp, _) = &p else { panic!() };
        let h = hash("sha256", b"hello").unwrap();
        let scheme = rsa::Pkcs1v15Sign { hash_len: Some(32), prefix: digest_info_prefix("sha256").unwrap()[..19].into() };
        rp.verify(scheme, &h, &sig).unwrap();
        for salt in [SALTLEN_DIGEST, SALTLEN_MAX_SIGN, 10] {
            let pss = SignOpts { padding: Some(RSA_PKCS1_PSS_PADDING), salt_len: Some(salt), p1363: false };
            let sig = sign(Some("sha256"), &k, b"hello", &pss).unwrap();
            let auto = SignOpts { padding: Some(RSA_PKCS1_PSS_PADDING), salt_len: None, p1363: false };
            assert!(verify(Some("sha256"), &p, b"hello", &sig, &auto).unwrap());
            assert!(!verify(Some("sha256"), &p, b"hellO", &sig, &auto).unwrap());
            // rsa crate's PSS verifier agrees for the digest-length salt.
            if salt == SALTLEN_DIGEST {
                rp.verify(rsa::Pss::new::<sha2::Sha256>(), &h, &sig).unwrap();
            }
        }
        let ct = rsa_crypt("publicEncrypt", &Imported::Public(p), b"secret", RSA_PKCS1_OAEP_PADDING, "sha1", None).unwrap();
        let k2 = Imported::Private(PrivKey::Rsa(rk.clone(), false));
        assert_eq!(rsa_crypt("privateDecrypt", &k2, &ct, RSA_PKCS1_OAEP_PADDING, "sha1", None).unwrap(), b"secret");
        let s = rsa_crypt("privateEncrypt", &k2, b"raw", RSA_PKCS1_PADDING, "sha1", None).unwrap();
        assert_eq!(rsa_crypt("publicDecrypt", &k2, &s, RSA_PKCS1_PADDING, "sha1", None).unwrap(), b"raw");
        // PKCS#1 PEM, encrypted legacy PEM and encrypted PKCS#8 round trips.
        for (typ, cipher) in [("pkcs1", None), ("pkcs1", Some("aes-256-cbc")), ("pkcs8", Some("aes-256-cbc"))] {
            let Exported::Text(pem) = export_private(&k, "pem", typ, cipher, cipher.map(|_| b"pw".as_slice())).unwrap() else { panic!() };
            let Imported::Private(back) = import(pem.as_bytes(), "pem", "", cipher.map(|_| b"pw".as_slice()), true).unwrap() else { panic!() };
            assert_eq!(back.to_pkcs8(), k.to_pkcs8(), "{typ} {cipher:?}");
            if cipher.is_some() {
                assert_eq!(import(pem.as_bytes(), "pem", "", None, true).err().unwrap().code, Some("ERR_OSSL_CRYPTO_INTERRUPTED_OR_CANCELLED"));
                assert_eq!(import(pem.as_bytes(), "pem", "", Some(b"bad"), true).err().unwrap().code, Some("ERR_OSSL_BAD_DECRYPT"));
            }
        }
    }

    #[test]
    fn x25519_and_errors() {
        let a = generate("x25519", 0, 0, None, None).unwrap();
        let b = generate("x25519", 0, 0, None, None).unwrap();
        assert_eq!(derive(&a, &b.public()).unwrap(), derive(&b, &a.public()).unwrap());
        assert_eq!(import(b"garbage", "pem", "", None, true).err().unwrap().code, Some("ERR_OSSL_UNSUPPORTED"));
        let e = import(b"-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----", "pem", "", None, false).err().unwrap();
        assert_eq!(e.code, Some("ERR_OSSL_ASN1_WRONG_TAG"));
    }
}
