//! Symmetric ciphers for `crypto.createCipheriv`/`createDecipheriv` (and WebCrypto AES): every
//! cipher Node 22's OpenSSL default provider offers (`crypto.getCiphers()`): AES, ARIA,
//! Camellia, SM4 and Triple-DES block ciphers in ECB, CBC, CFB (128/8/1), OFB, CTR, GCM, CCM,
//! OCB and XTS modes, AES key wrap (RFC 3394/5649), CMS Triple-DES key wrap (RFC 3217),
//! ChaCha20 and ChaCha20-Poly1305. Modes are implemented here on top of the RustCrypto block
//! ciphers so they stream like OpenSSL (`update` returns output as soon as it is available).
//! Ciphers only in OpenSSL's legacy provider (Blowfish, CAST5, DES, IDEA, RC2, RC4, SEED) fail
//! like they do in Node 22 (`ERR_OSSL_EVP_UNSUPPORTED`).

use crate::js::{Context, JsArgs, JsResult, JsValue};
use cipher::generic_array::GenericArray;
use cipher::{BlockDecrypt, BlockEncrypt, KeyInit, KeyIvInit, StreamCipher, StreamCipherSeek};
use universal_hash::UniversalHash;

use super::{bytes_arg, bytes_to_js, coded_error, native_object, with_native};
use crate::engine::{OpFn, arg_string, js_str};

/// A cipher failure with Node's message, code and error class.
#[derive(Debug, Clone)]
pub(crate) struct CipherError {
    pub kind: &'static str,
    pub message: String,
    pub code: Option<&'static str>,
}

impl CipherError {
    pub(crate) fn new(kind: &'static str, message: &str, code: Option<&'static str>) -> Self {
        Self { kind, message: message.into(), code }
    }
    fn bad_decrypt() -> Self {
        Self::new("Error", "error:1C800064:Provider routines::bad decrypt", Some("ERR_OSSL_BAD_DECRYPT"))
    }
    fn wrong_final() -> Self {
        Self::new("Error", "error:1C80006B:Provider routines::wrong final block length", Some("ERR_OSSL_WRONG_FINAL_BLOCK_LENGTH"))
    }
    fn auth() -> Self {
        Self::new("Error", "Unsupported state or unable to authenticate data", None)
    }
    /// Node's message when OpenSSL's `EVP_CipherUpdate` fails.
    fn update_state() -> Self {
        Self::new("Error", "Trying to add data in unsupported state", None)
    }
    fn bad_tag_len(n: usize) -> Self {
        Self::new("TypeError", &format!("Invalid authentication tag length: {n}"), Some("ERR_CRYPTO_INVALID_AUTH_TAG"))
    }
    pub(crate) fn throw(&self, ctx: &mut Context) -> crate::js::JsError {
        let props: Vec<(&str, JsValue)> = self.code.map(|c| vec![("code", js_str(c))]).unwrap_or_default();
        coded_error(ctx, self.kind, &self.message, &props)
    }
}

type CResult<T> = Result<T, CipherError>;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Alg {
    Aes,
    Aria,
    Camellia,
    Sm4,
    Des3,
    Des2,
    ChaCha,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum M {
    Cbc,
    Ecb,
    Ctr,
    Cfb,
    Cfb8,
    Cfb1,
    Ofb,
    Gcm,
    Ccm,
    Ocb,
    Xts,
    Wrap,
    WrapPad,
    Des3Wrap,
    ChaCha,
    ChaChaPoly,
    /// TLS-only "stitched" AES-CBC-HMAC ciphers: listed, but `update` fails as in Node.
    HmacStitched,
}

/// A cipher as `getCipherInfo` reports it, plus how to run it.
#[derive(Clone, Copy, Debug)]
struct Spec {
    canonical: &'static str,
    nid: u32,
    key: usize,
    iv: usize,
    bs: usize,
    info_mode: &'static str,
    alg: Alg,
    mode: M,
}

/// Node 22's `crypto.getCiphers()` (lowercased) with OpenSSL's `getCipherInfo` data.
const SPECS: &[(&str, Spec)] = &[
    ("aes-128-cbc", Spec { canonical: "aes-128-cbc", nid: 419, key: 16, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aes, mode: M::Cbc }),
    ("aes-128-cbc-hmac-sha1", Spec { canonical: "aes-128-cbc-hmac-sha1", nid: 916, key: 16, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aes, mode: M::HmacStitched }),
    ("aes-128-cbc-hmac-sha256", Spec { canonical: "aes-128-cbc-hmac-sha256", nid: 948, key: 16, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aes, mode: M::HmacStitched }),
    ("aes-128-ccm", Spec { canonical: "id-aes128-ccm", nid: 896, key: 16, iv: 12, bs: 1, info_mode: "ccm", alg: Alg::Aes, mode: M::Ccm }),
    ("aes-128-cfb", Spec { canonical: "aes-128-cfb", nid: 421, key: 16, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aes, mode: M::Cfb }),
    ("aes-128-cfb1", Spec { canonical: "aes-128-cfb1", nid: 650, key: 16, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aes, mode: M::Cfb1 }),
    ("aes-128-cfb8", Spec { canonical: "aes-128-cfb8", nid: 653, key: 16, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aes, mode: M::Cfb8 }),
    ("aes-128-ctr", Spec { canonical: "aes-128-ctr", nid: 904, key: 16, iv: 16, bs: 1, info_mode: "ctr", alg: Alg::Aes, mode: M::Ctr }),
    ("aes-128-ecb", Spec { canonical: "aes-128-ecb", nid: 418, key: 16, iv: 0, bs: 16, info_mode: "ecb", alg: Alg::Aes, mode: M::Ecb }),
    ("aes-128-gcm", Spec { canonical: "id-aes128-gcm", nid: 895, key: 16, iv: 12, bs: 1, info_mode: "gcm", alg: Alg::Aes, mode: M::Gcm }),
    ("aes-128-ocb", Spec { canonical: "aes-128-ocb", nid: 958, key: 16, iv: 12, bs: 16, info_mode: "ocb", alg: Alg::Aes, mode: M::Ocb }),
    ("aes-128-ofb", Spec { canonical: "aes-128-ofb", nid: 420, key: 16, iv: 16, bs: 1, info_mode: "ofb", alg: Alg::Aes, mode: M::Ofb }),
    ("aes-128-xts", Spec { canonical: "aes-128-xts", nid: 913, key: 32, iv: 16, bs: 1, info_mode: "xts", alg: Alg::Aes, mode: M::Xts }),
    ("aes-192-cbc", Spec { canonical: "aes-192-cbc", nid: 423, key: 24, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aes, mode: M::Cbc }),
    ("aes-192-ccm", Spec { canonical: "id-aes192-ccm", nid: 899, key: 24, iv: 12, bs: 1, info_mode: "ccm", alg: Alg::Aes, mode: M::Ccm }),
    ("aes-192-cfb", Spec { canonical: "aes-192-cfb", nid: 425, key: 24, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aes, mode: M::Cfb }),
    ("aes-192-cfb1", Spec { canonical: "aes-192-cfb1", nid: 651, key: 24, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aes, mode: M::Cfb1 }),
    ("aes-192-cfb8", Spec { canonical: "aes-192-cfb8", nid: 654, key: 24, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aes, mode: M::Cfb8 }),
    ("aes-192-ctr", Spec { canonical: "aes-192-ctr", nid: 905, key: 24, iv: 16, bs: 1, info_mode: "ctr", alg: Alg::Aes, mode: M::Ctr }),
    ("aes-192-ecb", Spec { canonical: "aes-192-ecb", nid: 422, key: 24, iv: 0, bs: 16, info_mode: "ecb", alg: Alg::Aes, mode: M::Ecb }),
    ("aes-192-gcm", Spec { canonical: "id-aes192-gcm", nid: 898, key: 24, iv: 12, bs: 1, info_mode: "gcm", alg: Alg::Aes, mode: M::Gcm }),
    ("aes-192-ocb", Spec { canonical: "aes-192-ocb", nid: 959, key: 24, iv: 12, bs: 16, info_mode: "ocb", alg: Alg::Aes, mode: M::Ocb }),
    ("aes-192-ofb", Spec { canonical: "aes-192-ofb", nid: 424, key: 24, iv: 16, bs: 1, info_mode: "ofb", alg: Alg::Aes, mode: M::Ofb }),
    ("aes-256-cbc", Spec { canonical: "aes-256-cbc", nid: 427, key: 32, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aes, mode: M::Cbc }),
    ("aes-256-cbc-hmac-sha1", Spec { canonical: "aes-256-cbc-hmac-sha1", nid: 918, key: 32, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aes, mode: M::HmacStitched }),
    ("aes-256-cbc-hmac-sha256", Spec { canonical: "aes-256-cbc-hmac-sha256", nid: 950, key: 32, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aes, mode: M::HmacStitched }),
    ("aes-256-ccm", Spec { canonical: "id-aes256-ccm", nid: 902, key: 32, iv: 12, bs: 1, info_mode: "ccm", alg: Alg::Aes, mode: M::Ccm }),
    ("aes-256-cfb", Spec { canonical: "aes-256-cfb", nid: 429, key: 32, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aes, mode: M::Cfb }),
    ("aes-256-cfb1", Spec { canonical: "aes-256-cfb1", nid: 652, key: 32, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aes, mode: M::Cfb1 }),
    ("aes-256-cfb8", Spec { canonical: "aes-256-cfb8", nid: 655, key: 32, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aes, mode: M::Cfb8 }),
    ("aes-256-ctr", Spec { canonical: "aes-256-ctr", nid: 906, key: 32, iv: 16, bs: 1, info_mode: "ctr", alg: Alg::Aes, mode: M::Ctr }),
    ("aes-256-ecb", Spec { canonical: "aes-256-ecb", nid: 426, key: 32, iv: 0, bs: 16, info_mode: "ecb", alg: Alg::Aes, mode: M::Ecb }),
    ("aes-256-gcm", Spec { canonical: "id-aes256-gcm", nid: 901, key: 32, iv: 12, bs: 1, info_mode: "gcm", alg: Alg::Aes, mode: M::Gcm }),
    ("aes-256-ocb", Spec { canonical: "aes-256-ocb", nid: 960, key: 32, iv: 12, bs: 16, info_mode: "ocb", alg: Alg::Aes, mode: M::Ocb }),
    ("aes-256-ofb", Spec { canonical: "aes-256-ofb", nid: 428, key: 32, iv: 16, bs: 1, info_mode: "ofb", alg: Alg::Aes, mode: M::Ofb }),
    ("aes-256-xts", Spec { canonical: "aes-256-xts", nid: 914, key: 64, iv: 16, bs: 1, info_mode: "xts", alg: Alg::Aes, mode: M::Xts }),
    ("aes128", Spec { canonical: "aes-128-cbc", nid: 419, key: 16, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aes, mode: M::Cbc }),
    ("aes128-wrap", Spec { canonical: "id-aes128-wrap", nid: 788, key: 16, iv: 8, bs: 8, info_mode: "wrap", alg: Alg::Aes, mode: M::Wrap }),
    ("aes128-wrap-pad", Spec { canonical: "id-aes128-wrap-pad", nid: 897, key: 16, iv: 4, bs: 8, info_mode: "wrap", alg: Alg::Aes, mode: M::WrapPad }),
    ("aes192", Spec { canonical: "aes-192-cbc", nid: 423, key: 24, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aes, mode: M::Cbc }),
    ("aes192-wrap", Spec { canonical: "id-aes192-wrap", nid: 789, key: 24, iv: 8, bs: 8, info_mode: "wrap", alg: Alg::Aes, mode: M::Wrap }),
    ("aes192-wrap-pad", Spec { canonical: "id-aes192-wrap-pad", nid: 900, key: 24, iv: 4, bs: 8, info_mode: "wrap", alg: Alg::Aes, mode: M::WrapPad }),
    ("aes256", Spec { canonical: "aes-256-cbc", nid: 427, key: 32, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aes, mode: M::Cbc }),
    ("aes256-wrap", Spec { canonical: "id-aes256-wrap", nid: 790, key: 32, iv: 8, bs: 8, info_mode: "wrap", alg: Alg::Aes, mode: M::Wrap }),
    ("aes256-wrap-pad", Spec { canonical: "id-aes256-wrap-pad", nid: 903, key: 32, iv: 4, bs: 8, info_mode: "wrap", alg: Alg::Aes, mode: M::WrapPad }),
    ("aria-128-cbc", Spec { canonical: "aria-128-cbc", nid: 1066, key: 16, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aria, mode: M::Cbc }),
    ("aria-128-ccm", Spec { canonical: "aria-128-ccm", nid: 1120, key: 16, iv: 12, bs: 1, info_mode: "ccm", alg: Alg::Aria, mode: M::Ccm }),
    ("aria-128-cfb", Spec { canonical: "aria-128-cfb", nid: 1067, key: 16, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aria, mode: M::Cfb }),
    ("aria-128-cfb1", Spec { canonical: "aria-128-cfb1", nid: 1080, key: 16, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aria, mode: M::Cfb1 }),
    ("aria-128-cfb8", Spec { canonical: "aria-128-cfb8", nid: 1083, key: 16, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aria, mode: M::Cfb8 }),
    ("aria-128-ctr", Spec { canonical: "aria-128-ctr", nid: 1069, key: 16, iv: 16, bs: 1, info_mode: "ctr", alg: Alg::Aria, mode: M::Ctr }),
    ("aria-128-ecb", Spec { canonical: "aria-128-ecb", nid: 1065, key: 16, iv: 0, bs: 16, info_mode: "ecb", alg: Alg::Aria, mode: M::Ecb }),
    ("aria-128-gcm", Spec { canonical: "aria-128-gcm", nid: 1123, key: 16, iv: 12, bs: 1, info_mode: "gcm", alg: Alg::Aria, mode: M::Gcm }),
    ("aria-128-ofb", Spec { canonical: "aria-128-ofb", nid: 1068, key: 16, iv: 16, bs: 1, info_mode: "ofb", alg: Alg::Aria, mode: M::Ofb }),
    ("aria-192-cbc", Spec { canonical: "aria-192-cbc", nid: 1071, key: 24, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aria, mode: M::Cbc }),
    ("aria-192-ccm", Spec { canonical: "aria-192-ccm", nid: 1121, key: 24, iv: 12, bs: 1, info_mode: "ccm", alg: Alg::Aria, mode: M::Ccm }),
    ("aria-192-cfb", Spec { canonical: "aria-192-cfb", nid: 1072, key: 24, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aria, mode: M::Cfb }),
    ("aria-192-cfb1", Spec { canonical: "aria-192-cfb1", nid: 1081, key: 24, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aria, mode: M::Cfb1 }),
    ("aria-192-cfb8", Spec { canonical: "aria-192-cfb8", nid: 1084, key: 24, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aria, mode: M::Cfb8 }),
    ("aria-192-ctr", Spec { canonical: "aria-192-ctr", nid: 1074, key: 24, iv: 16, bs: 1, info_mode: "ctr", alg: Alg::Aria, mode: M::Ctr }),
    ("aria-192-ecb", Spec { canonical: "aria-192-ecb", nid: 1070, key: 24, iv: 0, bs: 16, info_mode: "ecb", alg: Alg::Aria, mode: M::Ecb }),
    ("aria-192-gcm", Spec { canonical: "aria-192-gcm", nid: 1124, key: 24, iv: 12, bs: 1, info_mode: "gcm", alg: Alg::Aria, mode: M::Gcm }),
    ("aria-192-ofb", Spec { canonical: "aria-192-ofb", nid: 1073, key: 24, iv: 16, bs: 1, info_mode: "ofb", alg: Alg::Aria, mode: M::Ofb }),
    ("aria-256-cbc", Spec { canonical: "aria-256-cbc", nid: 1076, key: 32, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aria, mode: M::Cbc }),
    ("aria-256-ccm", Spec { canonical: "aria-256-ccm", nid: 1122, key: 32, iv: 12, bs: 1, info_mode: "ccm", alg: Alg::Aria, mode: M::Ccm }),
    ("aria-256-cfb", Spec { canonical: "aria-256-cfb", nid: 1077, key: 32, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aria, mode: M::Cfb }),
    ("aria-256-cfb1", Spec { canonical: "aria-256-cfb1", nid: 1082, key: 32, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aria, mode: M::Cfb1 }),
    ("aria-256-cfb8", Spec { canonical: "aria-256-cfb8", nid: 1085, key: 32, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Aria, mode: M::Cfb8 }),
    ("aria-256-ctr", Spec { canonical: "aria-256-ctr", nid: 1079, key: 32, iv: 16, bs: 1, info_mode: "ctr", alg: Alg::Aria, mode: M::Ctr }),
    ("aria-256-ecb", Spec { canonical: "aria-256-ecb", nid: 1075, key: 32, iv: 0, bs: 16, info_mode: "ecb", alg: Alg::Aria, mode: M::Ecb }),
    ("aria-256-gcm", Spec { canonical: "aria-256-gcm", nid: 1125, key: 32, iv: 12, bs: 1, info_mode: "gcm", alg: Alg::Aria, mode: M::Gcm }),
    ("aria-256-ofb", Spec { canonical: "aria-256-ofb", nid: 1078, key: 32, iv: 16, bs: 1, info_mode: "ofb", alg: Alg::Aria, mode: M::Ofb }),
    ("aria128", Spec { canonical: "aria-128-cbc", nid: 1066, key: 16, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aria, mode: M::Cbc }),
    ("aria192", Spec { canonical: "aria-192-cbc", nid: 1071, key: 24, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aria, mode: M::Cbc }),
    ("aria256", Spec { canonical: "aria-256-cbc", nid: 1076, key: 32, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Aria, mode: M::Cbc }),
    ("camellia-128-cbc", Spec { canonical: "camellia-128-cbc", nid: 751, key: 16, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Camellia, mode: M::Cbc }),
    ("camellia-128-cfb", Spec { canonical: "camellia-128-cfb", nid: 757, key: 16, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Camellia, mode: M::Cfb }),
    ("camellia-128-cfb1", Spec { canonical: "camellia-128-cfb1", nid: 760, key: 16, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Camellia, mode: M::Cfb1 }),
    ("camellia-128-cfb8", Spec { canonical: "camellia-128-cfb8", nid: 763, key: 16, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Camellia, mode: M::Cfb8 }),
    ("camellia-128-ctr", Spec { canonical: "camellia-128-ctr", nid: 963, key: 16, iv: 16, bs: 1, info_mode: "ctr", alg: Alg::Camellia, mode: M::Ctr }),
    ("camellia-128-ecb", Spec { canonical: "camellia-128-ecb", nid: 754, key: 16, iv: 0, bs: 16, info_mode: "ecb", alg: Alg::Camellia, mode: M::Ecb }),
    ("camellia-128-ofb", Spec { canonical: "camellia-128-ofb", nid: 766, key: 16, iv: 16, bs: 1, info_mode: "ofb", alg: Alg::Camellia, mode: M::Ofb }),
    ("camellia-192-cbc", Spec { canonical: "camellia-192-cbc", nid: 752, key: 24, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Camellia, mode: M::Cbc }),
    ("camellia-192-cfb", Spec { canonical: "camellia-192-cfb", nid: 758, key: 24, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Camellia, mode: M::Cfb }),
    ("camellia-192-cfb1", Spec { canonical: "camellia-192-cfb1", nid: 761, key: 24, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Camellia, mode: M::Cfb1 }),
    ("camellia-192-cfb8", Spec { canonical: "camellia-192-cfb8", nid: 764, key: 24, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Camellia, mode: M::Cfb8 }),
    ("camellia-192-ctr", Spec { canonical: "camellia-192-ctr", nid: 967, key: 24, iv: 16, bs: 1, info_mode: "ctr", alg: Alg::Camellia, mode: M::Ctr }),
    ("camellia-192-ecb", Spec { canonical: "camellia-192-ecb", nid: 755, key: 24, iv: 0, bs: 16, info_mode: "ecb", alg: Alg::Camellia, mode: M::Ecb }),
    ("camellia-192-ofb", Spec { canonical: "camellia-192-ofb", nid: 767, key: 24, iv: 16, bs: 1, info_mode: "ofb", alg: Alg::Camellia, mode: M::Ofb }),
    ("camellia-256-cbc", Spec { canonical: "camellia-256-cbc", nid: 753, key: 32, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Camellia, mode: M::Cbc }),
    ("camellia-256-cfb", Spec { canonical: "camellia-256-cfb", nid: 759, key: 32, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Camellia, mode: M::Cfb }),
    ("camellia-256-cfb1", Spec { canonical: "camellia-256-cfb1", nid: 762, key: 32, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Camellia, mode: M::Cfb1 }),
    ("camellia-256-cfb8", Spec { canonical: "camellia-256-cfb8", nid: 765, key: 32, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Camellia, mode: M::Cfb8 }),
    ("camellia-256-ctr", Spec { canonical: "camellia-256-ctr", nid: 971, key: 32, iv: 16, bs: 1, info_mode: "ctr", alg: Alg::Camellia, mode: M::Ctr }),
    ("camellia-256-ecb", Spec { canonical: "camellia-256-ecb", nid: 756, key: 32, iv: 0, bs: 16, info_mode: "ecb", alg: Alg::Camellia, mode: M::Ecb }),
    ("camellia-256-ofb", Spec { canonical: "camellia-256-ofb", nid: 768, key: 32, iv: 16, bs: 1, info_mode: "ofb", alg: Alg::Camellia, mode: M::Ofb }),
    ("camellia128", Spec { canonical: "camellia-128-cbc", nid: 751, key: 16, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Camellia, mode: M::Cbc }),
    ("camellia192", Spec { canonical: "camellia-192-cbc", nid: 752, key: 24, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Camellia, mode: M::Cbc }),
    ("camellia256", Spec { canonical: "camellia-256-cbc", nid: 753, key: 32, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Camellia, mode: M::Cbc }),
    ("chacha20", Spec { canonical: "chacha20", nid: 1019, key: 32, iv: 16, bs: 0, info_mode: "stream", alg: Alg::ChaCha, mode: M::ChaCha }),
    ("chacha20-poly1305", Spec { canonical: "chacha20-poly1305", nid: 1018, key: 32, iv: 12, bs: 0, info_mode: "stream", alg: Alg::ChaCha, mode: M::ChaChaPoly }),
    ("des-ede", Spec { canonical: "des-ede", nid: 32, key: 16, iv: 0, bs: 8, info_mode: "ecb", alg: Alg::Des2, mode: M::Ecb }),
    ("des-ede-cbc", Spec { canonical: "des-ede-cbc", nid: 43, key: 16, iv: 8, bs: 8, info_mode: "cbc", alg: Alg::Des2, mode: M::Cbc }),
    ("des-ede-cfb", Spec { canonical: "des-ede-cfb", nid: 60, key: 16, iv: 8, bs: 1, info_mode: "cfb", alg: Alg::Des2, mode: M::Cfb }),
    ("des-ede-ecb", Spec { canonical: "des-ede", nid: 32, key: 16, iv: 0, bs: 8, info_mode: "ecb", alg: Alg::Des2, mode: M::Ecb }),
    ("des-ede-ofb", Spec { canonical: "des-ede-ofb", nid: 62, key: 16, iv: 8, bs: 1, info_mode: "ofb", alg: Alg::Des2, mode: M::Ofb }),
    ("des-ede3", Spec { canonical: "des-ede3", nid: 33, key: 24, iv: 0, bs: 8, info_mode: "ecb", alg: Alg::Des3, mode: M::Ecb }),
    ("des-ede3-cbc", Spec { canonical: "des-ede3-cbc", nid: 44, key: 24, iv: 8, bs: 8, info_mode: "cbc", alg: Alg::Des3, mode: M::Cbc }),
    ("des-ede3-cfb", Spec { canonical: "des-ede3-cfb", nid: 61, key: 24, iv: 8, bs: 1, info_mode: "cfb", alg: Alg::Des3, mode: M::Cfb }),
    ("des-ede3-cfb1", Spec { canonical: "des-ede3-cfb1", nid: 658, key: 24, iv: 8, bs: 1, info_mode: "cfb", alg: Alg::Des3, mode: M::Cfb1 }),
    ("des-ede3-cfb8", Spec { canonical: "des-ede3-cfb8", nid: 659, key: 24, iv: 8, bs: 1, info_mode: "cfb", alg: Alg::Des3, mode: M::Cfb8 }),
    ("des-ede3-ecb", Spec { canonical: "des-ede3", nid: 33, key: 24, iv: 0, bs: 8, info_mode: "ecb", alg: Alg::Des3, mode: M::Ecb }),
    ("des-ede3-ofb", Spec { canonical: "des-ede3-ofb", nid: 63, key: 24, iv: 8, bs: 1, info_mode: "ofb", alg: Alg::Des3, mode: M::Ofb }),
    ("des3", Spec { canonical: "des-ede3-cbc", nid: 44, key: 24, iv: 8, bs: 8, info_mode: "cbc", alg: Alg::Des3, mode: M::Cbc }),
    ("des3-wrap", Spec { canonical: "id-smime-alg-cms3deswrap", nid: 246, key: 24, iv: 0, bs: 8, info_mode: "wrap", alg: Alg::Des3, mode: M::Des3Wrap }),
    ("id-aes128-ccm", Spec { canonical: "id-aes128-ccm", nid: 896, key: 16, iv: 12, bs: 1, info_mode: "ccm", alg: Alg::Aes, mode: M::Ccm }),
    ("id-aes128-gcm", Spec { canonical: "id-aes128-gcm", nid: 895, key: 16, iv: 12, bs: 1, info_mode: "gcm", alg: Alg::Aes, mode: M::Gcm }),
    ("id-aes128-wrap", Spec { canonical: "id-aes128-wrap", nid: 788, key: 16, iv: 8, bs: 8, info_mode: "wrap", alg: Alg::Aes, mode: M::Wrap }),
    ("id-aes128-wrap-pad", Spec { canonical: "id-aes128-wrap-pad", nid: 897, key: 16, iv: 4, bs: 8, info_mode: "wrap", alg: Alg::Aes, mode: M::WrapPad }),
    ("id-aes192-ccm", Spec { canonical: "id-aes192-ccm", nid: 899, key: 24, iv: 12, bs: 1, info_mode: "ccm", alg: Alg::Aes, mode: M::Ccm }),
    ("id-aes192-gcm", Spec { canonical: "id-aes192-gcm", nid: 898, key: 24, iv: 12, bs: 1, info_mode: "gcm", alg: Alg::Aes, mode: M::Gcm }),
    ("id-aes192-wrap", Spec { canonical: "id-aes192-wrap", nid: 789, key: 24, iv: 8, bs: 8, info_mode: "wrap", alg: Alg::Aes, mode: M::Wrap }),
    ("id-aes192-wrap-pad", Spec { canonical: "id-aes192-wrap-pad", nid: 900, key: 24, iv: 4, bs: 8, info_mode: "wrap", alg: Alg::Aes, mode: M::WrapPad }),
    ("id-aes256-ccm", Spec { canonical: "id-aes256-ccm", nid: 902, key: 32, iv: 12, bs: 1, info_mode: "ccm", alg: Alg::Aes, mode: M::Ccm }),
    ("id-aes256-gcm", Spec { canonical: "id-aes256-gcm", nid: 901, key: 32, iv: 12, bs: 1, info_mode: "gcm", alg: Alg::Aes, mode: M::Gcm }),
    ("id-aes256-wrap", Spec { canonical: "id-aes256-wrap", nid: 790, key: 32, iv: 8, bs: 8, info_mode: "wrap", alg: Alg::Aes, mode: M::Wrap }),
    ("id-aes256-wrap-pad", Spec { canonical: "id-aes256-wrap-pad", nid: 903, key: 32, iv: 4, bs: 8, info_mode: "wrap", alg: Alg::Aes, mode: M::WrapPad }),
    ("id-smime-alg-cms3deswrap", Spec { canonical: "id-smime-alg-cms3deswrap", nid: 246, key: 24, iv: 0, bs: 8, info_mode: "wrap", alg: Alg::Des3, mode: M::Des3Wrap }),
    ("sm4", Spec { canonical: "sm4-cbc", nid: 1134, key: 16, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Sm4, mode: M::Cbc }),
    ("sm4-cbc", Spec { canonical: "sm4-cbc", nid: 1134, key: 16, iv: 16, bs: 16, info_mode: "cbc", alg: Alg::Sm4, mode: M::Cbc }),
    ("sm4-cfb", Spec { canonical: "sm4-cfb", nid: 1137, key: 16, iv: 16, bs: 1, info_mode: "cfb", alg: Alg::Sm4, mode: M::Cfb }),
    ("sm4-ctr", Spec { canonical: "sm4-ctr", nid: 1139, key: 16, iv: 16, bs: 1, info_mode: "ctr", alg: Alg::Sm4, mode: M::Ctr }),
    ("sm4-ecb", Spec { canonical: "sm4-ecb", nid: 1133, key: 16, iv: 0, bs: 16, info_mode: "ecb", alg: Alg::Sm4, mode: M::Ecb }),
    ("sm4-ofb", Spec { canonical: "sm4-ofb", nid: 1135, key: 16, iv: 16, bs: 1, info_mode: "ofb", alg: Alg::Sm4, mode: M::Ofb }),
];

fn lookup(name: &str) -> Option<&'static Spec> {
    let n = name.to_ascii_lowercase();
    SPECS.iter().find(|(alias, _)| *alias == n).map(|(_, s)| s)
}

/// Ciphers OpenSSL 3 has only in its legacy provider, which Node does not load.
fn legacy_cipher(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["bf", "blowfish", "cast", "desx", "idea", "rc2", "rc4", "rc5", "seed"].iter().any(|p| n.starts_with(p))
        || n == "des"
        || ["des-cbc", "des-cfb", "des-cfb1", "des-cfb8", "des-ecb", "des-ofb"].contains(&n.as_str())
}

/// Cipher names reported by `crypto.getCiphers()` (Node's spelling).
pub(crate) fn cipher_names() -> Vec<&'static str> {
    SPECS
        .iter()
        .map(|(alias, spec)| {
            // Node lists `id-aes128-CCM`/`id-aes128-GCM` and `id-smime-alg-CMS3DESwrap` in OpenSSL's case.
            match *alias {
                "id-aes128-ccm" => "id-aes128-CCM",
                "id-aes192-ccm" => "id-aes192-CCM",
                "id-aes256-ccm" => "id-aes256-CCM",
                "id-aes128-gcm" => "id-aes128-GCM",
                "id-aes192-gcm" => "id-aes192-GCM",
                "id-aes256-gcm" => "id-aes256-GCM",
                "id-smime-alg-cms3deswrap" => "id-smime-alg-CMS3DESwrap",
                _ => {
                    let _ = spec;
                    alias
                }
            }
        })
        .collect()
}

enum Block {
    A128(aes::Aes128),
    A192(aes::Aes192),
    A256(aes::Aes256),
    Aria128(aria::Aria128),
    Aria192(aria::Aria192),
    Aria256(aria::Aria256),
    Cam128(camellia::Camellia128),
    Cam192(camellia::Camellia192),
    Cam256(camellia::Camellia256),
    Sm4(sm4::Sm4),
    Des3(des::TdesEde3),
    Des2(des::TdesEde2),
}

macro_rules! block_dispatch {
    ($self:expr, $c:ident => $body:expr) => {
        match $self {
            Block::A128($c) => $body,
            Block::A192($c) => $body,
            Block::A256($c) => $body,
            Block::Aria128($c) => $body,
            Block::Aria192($c) => $body,
            Block::Aria256($c) => $body,
            Block::Cam128($c) => $body,
            Block::Cam192($c) => $body,
            Block::Cam256($c) => $body,
            Block::Sm4($c) => $body,
            Block::Des3($c) => $body,
            Block::Des2($c) => $body,
        }
    };
}

impl Block {
    fn new(alg: Alg, key: &[u8]) -> Self {
        let k = key;
        match (alg, key.len()) {
            (Alg::Aes, 16) => Block::A128(aes::Aes128::new_from_slice(k).expect("key length checked")),
            (Alg::Aes, 24) => Block::A192(aes::Aes192::new_from_slice(k).expect("key length checked")),
            (Alg::Aes, _) => Block::A256(aes::Aes256::new_from_slice(k).expect("key length checked")),
            (Alg::Aria, 16) => Block::Aria128(aria::Aria128::new_from_slice(k).expect("key length checked")),
            (Alg::Aria, 24) => Block::Aria192(aria::Aria192::new_from_slice(k).expect("key length checked")),
            (Alg::Aria, _) => Block::Aria256(aria::Aria256::new_from_slice(k).expect("key length checked")),
            (Alg::Camellia, 16) => Block::Cam128(camellia::Camellia128::new_from_slice(k).expect("key length checked")),
            (Alg::Camellia, 24) => Block::Cam192(camellia::Camellia192::new_from_slice(k).expect("key length checked")),
            (Alg::Camellia, _) => Block::Cam256(camellia::Camellia256::new_from_slice(k).expect("key length checked")),
            (Alg::Sm4, _) => Block::Sm4(sm4::Sm4::new_from_slice(k).expect("key length checked")),
            (Alg::Des2, _) => Block::Des2(des::TdesEde2::new_from_slice(k).expect("key length checked")),
            _ => Block::Des3(des::TdesEde3::new_from_slice(k).expect("key length checked")),
        }
    }
    fn aes(key: &[u8]) -> Self {
        Self::new(Alg::Aes, key)
    }
    fn size(&self) -> usize {
        if matches!(self, Block::Des3(_) | Block::Des2(_)) { 8 } else { 16 }
    }
    fn encrypt(&self, b: &mut [u8]) {
        block_dispatch!(self, c => c.encrypt_block(GenericArray::from_mut_slice(b)))
    }
    fn decrypt(&self, b: &mut [u8]) {
        block_dispatch!(self, c => c.decrypt_block(GenericArray::from_mut_slice(b)))
    }
    fn encrypted(&self, b: &[u8]) -> Vec<u8> {
        let mut out = b.to_vec();
        self.encrypt(&mut out);
        out
    }
}

/// Streaming MAC over (AAD, ciphertext) in GCM / ChaCha20-Poly1305 layout.
enum Mac {
    Ghash(Box<ghash::GHash>),
    Poly(Box<poly1305::Poly1305>),
}

struct AeadState {
    mac: Mac,
    aad_len: u64,
    ct_len: u64,
    /// Partial 16-byte block of AAD or ciphertext not yet absorbed.
    pending: Vec<u8>,
    in_ct: bool,
    tag_mask: [u8; 16],
    tag_len: usize,
}

impl AeadState {
    fn absorb(&mut self, data: &[u8]) {
        self.pending.extend_from_slice(data);
        let full = self.pending.len() / 16 * 16;
        if full > 0 {
            let blocks: Vec<_> = self.pending[..full].chunks(16).map(|c| *GenericArray::from_slice(c)).collect();
            match &mut self.mac {
                Mac::Ghash(g) => g.update(&blocks),
                Mac::Poly(p) => p.update(&blocks),
            }
            self.pending.drain(..full);
        }
    }
    fn pad(&mut self) {
        if !self.pending.is_empty() {
            let mut block = [0u8; 16];
            block[..self.pending.len()].copy_from_slice(&self.pending);
            self.pending.clear();
            let b = *GenericArray::from_slice(&block);
            match &mut self.mac {
                Mac::Ghash(g) => g.update(&[b]),
                Mac::Poly(p) => p.update(&[b]),
            }
        }
    }
    fn add_aad(&mut self, aad: &[u8]) -> CResult<()> {
        if self.in_ct {
            return Err(CipherError::new("Error", "Unsupported state", None));
        }
        self.aad_len += aad.len() as u64;
        self.absorb(aad);
        Ok(())
    }
    fn add_ct(&mut self, ct: &[u8]) {
        if !self.in_ct {
            self.pad();
            self.in_ct = true;
        }
        self.ct_len += ct.len() as u64;
        self.absorb(ct);
    }
    fn finish(&mut self) -> [u8; 16] {
        self.pad();
        let mut lens = [0u8; 16];
        let tag = match &mut self.mac {
            Mac::Ghash(g) => {
                lens[..8].copy_from_slice(&(self.aad_len * 8).to_be_bytes());
                lens[8..].copy_from_slice(&(self.ct_len * 8).to_be_bytes());
                g.update(&[*GenericArray::from_slice(&lens)]);
                std::mem::replace(g, Box::new(ghash::GHash::new(&Default::default()))).finalize()
            }
            Mac::Poly(p) => {
                lens[..8].copy_from_slice(&self.aad_len.to_le_bytes());
                lens[8..].copy_from_slice(&self.ct_len.to_le_bytes());
                p.update(&[*GenericArray::from_slice(&lens)]);
                std::mem::replace(p, Box::new(poly1305::Poly1305::new(&Default::default()))).finalize()
            }
        };
        let mut out = [0u8; 16];
        for i in 0..16 {
            out[i] = tag[i] ^ self.tag_mask[i];
        }
        out
    }
}

/// CCM (NIST SP 800-38C): one `update` call carries the whole message.
struct CcmState {
    block: Block,
    nonce: Vec<u8>,
    tag_len: usize,
    aad: Vec<u8>,
    plaintext_len: Option<usize>,
    done_update: bool,
    /// Decryption: the tag did not verify (reported by `final`).
    failed: bool,
}

impl CcmState {
    fn counter(&self, i: u64) -> [u8; 16] {
        let l = 15 - self.nonce.len();
        let mut a = [0u8; 16];
        a[0] = (l - 1) as u8;
        a[1..1 + self.nonce.len()].copy_from_slice(&self.nonce);
        let be = i.to_be_bytes();
        a[16 - l..].copy_from_slice(&be[8 - l..]);
        a
    }
    fn mac(&self, msg: &[u8]) -> [u8; 16] {
        let l = 15 - self.nonce.len();
        let mut b0 = [0u8; 16];
        b0[0] = (if self.aad.is_empty() { 0 } else { 0x40 }) | ((((self.tag_len - 2) / 2) as u8) << 3) | (l - 1) as u8;
        b0[1..1 + self.nonce.len()].copy_from_slice(&self.nonce);
        let be = (msg.len() as u64).to_be_bytes();
        b0[16 - l..].copy_from_slice(&be[8 - l..]);
        let mut x = b0;
        self.block.encrypt(&mut x);
        let absorb = |data: &[u8], x: &mut [u8; 16]| {
            for chunk in data.chunks(16) {
                for (a, b) in x.iter_mut().zip(chunk) {
                    *a ^= b;
                }
                self.block.encrypt(x);
            }
        };
        if !self.aad.is_empty() {
            let n = self.aad.len();
            let mut enc = if n < 0xff00 {
                (n as u16).to_be_bytes().to_vec()
            } else if n <= u32::MAX as usize {
                let mut v = vec![0xff, 0xfe];
                v.extend_from_slice(&(n as u32).to_be_bytes());
                v
            } else {
                let mut v = vec![0xff, 0xff];
                v.extend_from_slice(&(n as u64).to_be_bytes());
                v
            };
            enc.extend_from_slice(&self.aad);
            absorb(&enc, &mut x);
        }
        absorb(msg, &mut x);
        x
    }
    fn ctr(&self, data: &mut [u8]) {
        for (i, chunk) in data.chunks_mut(16).enumerate() {
            let mut ks = self.counter(i as u64 + 1);
            self.block.encrypt(&mut ks);
            for (a, b) in chunk.iter_mut().zip(ks) {
                *a ^= b;
            }
        }
    }
    fn tag(&self, mac: [u8; 16]) -> Vec<u8> {
        let mut s0 = self.counter(0);
        self.block.encrypt(&mut s0);
        mac.iter().zip(s0).take(self.tag_len).map(|(a, b)| a ^ b).collect()
    }
}

/// OCB3 (RFC 7253) over a 128-bit block cipher, streaming whole blocks.
struct OcbState {
    block: Block,
    l_star: [u8; 16],
    l_dollar: [u8; 16],
    l: Vec<[u8; 16]>,
    offset: [u8; 16],
    checksum: [u8; 16],
    blocks: u64,
    aad: Vec<u8>,
    tag_len: usize,
}

fn dbl(b: &[u8; 16]) -> [u8; 16] {
    let n = u128::from_be_bytes(*b);
    let mut r = n << 1;
    if n >> 127 == 1 {
        r ^= 0x87;
    }
    r.to_be_bytes()
}

fn xor16(a: &mut [u8; 16], b: &[u8; 16]) {
    for (x, y) in a.iter_mut().zip(b) {
        *x ^= y;
    }
}

impl OcbState {
    fn new(block: Block, nonce: &[u8], tag_len: usize) -> Self {
        let mut l_star = [0u8; 16];
        block.encrypt(&mut l_star);
        let l_dollar = dbl(&l_star);
        let mut l = vec![dbl(&l_dollar)];
        for _ in 1..64 {
            let next = dbl(l.last().expect("non-empty"));
            l.push(next);
        }
        let mut n = [0u8; 16];
        n[0] = (((tag_len * 8) % 128) as u8) << 1;
        n[16 - nonce.len()..].copy_from_slice(nonce);
        n[15 - nonce.len()] |= 1;
        let bottom = (n[15] & 0x3f) as usize;
        n[15] &= 0xc0;
        let mut ktop = n;
        block.encrypt(&mut ktop);
        let mut stretch = [0u8; 24];
        stretch[..16].copy_from_slice(&ktop);
        for i in 0..8 {
            stretch[16 + i] = ktop[i] ^ ktop[i + 1];
        }
        // Offset_0 = Stretch[1+bottom..128+bottom] (bits).
        let mut offset = [0u8; 16];
        let (byte, bit) = (bottom / 8, bottom % 8);
        for i in 0..16 {
            offset[i] = if bit == 0 { stretch[i + byte] } else { (stretch[i + byte] << bit) | (stretch[i + byte + 1] >> (8 - bit)) };
        }
        Self { block, l_star, l_dollar, l, offset, checksum: [0; 16], blocks: 0, aad: Vec::new(), tag_len }
    }
    fn process(&mut self, data: &[u8], encrypt: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        for chunk in data.chunks(16) {
            self.blocks += 1;
            let ntz = self.blocks.trailing_zeros() as usize;
            let l = self.l[ntz];
            xor16(&mut self.offset, &l);
            let mut b: [u8; 16] = chunk.try_into().expect("whole block");
            if encrypt {
                xor16(&mut self.checksum, &b);
            }
            xor16(&mut b, &self.offset);
            if encrypt { self.block.encrypt(&mut b) } else { self.block.decrypt(&mut b) }
            xor16(&mut b, &self.offset);
            if !encrypt {
                xor16(&mut self.checksum, &b);
            }
            out.extend_from_slice(&b);
        }
        out
    }
    fn last(&mut self, data: &[u8], encrypt: bool) -> Vec<u8> {
        if data.is_empty() {
            return Vec::new();
        }
        xor16(&mut self.offset, &self.l_star.clone());
        let mut pad = self.offset;
        self.block.encrypt(&mut pad);
        let out: Vec<u8> = data.iter().zip(pad).map(|(a, b)| a ^ b).collect();
        let plain = if encrypt { data } else { &out[..] };
        let mut p = [0u8; 16];
        p[..plain.len()].copy_from_slice(plain);
        p[plain.len()] = 0x80;
        xor16(&mut self.checksum, &p);
        out
    }
    fn hash_aad(&self) -> [u8; 16] {
        let mut sum = [0u8; 16];
        let mut offset = [0u8; 16];
        let full = self.aad.len() / 16;
        for i in 0..full {
            xor16(&mut offset, &self.l[(i as u64 + 1).trailing_zeros() as usize]);
            let mut b: [u8; 16] = self.aad[i * 16..i * 16 + 16].try_into().expect("whole block");
            xor16(&mut b, &offset);
            self.block.encrypt(&mut b);
            xor16(&mut sum, &b);
        }
        let rest = &self.aad[full * 16..];
        if !rest.is_empty() {
            xor16(&mut offset, &self.l_star);
            let mut b = [0u8; 16];
            b[..rest.len()].copy_from_slice(rest);
            b[rest.len()] = 0x80;
            xor16(&mut b, &offset);
            self.block.encrypt(&mut b);
            xor16(&mut sum, &b);
        }
        sum
    }
    fn tag(&self) -> Vec<u8> {
        let mut t = self.checksum;
        xor16(&mut t, &self.offset);
        xor16(&mut t, &self.l_dollar);
        self.block.encrypt(&mut t);
        xor16(&mut t, &self.hash_aad());
        t[..self.tag_len].to_vec()
    }
}

/// XTS (IEEE P1619) with ciphertext stealing; each `update` is one data unit, like OpenSSL.
fn xts(k1: &Block, k2: &Block, iv: &[u8], data: &[u8], encrypt: bool) -> CResult<Vec<u8>> {
    if data.len() < 16 {
        return Err(CipherError::update_state());
    }
    let mut t: [u8; 16] = iv.try_into().expect("iv length checked");
    k2.encrypt(&mut t);
    let next = |t: &[u8; 16]| -> [u8; 16] {
        let n = u128::from_le_bytes(*t);
        let mut r = n << 1;
        if n >> 127 == 1 {
            r ^= 0x87;
        }
        r.to_le_bytes()
    };
    let one = |b: &mut [u8; 16], t: &[u8; 16]| {
        xor16(b, t);
        if encrypt { k1.encrypt(b) } else { k1.decrypt(b) }
        xor16(b, t);
    };
    let mut out = data.to_vec();
    let full = data.len() / 16;
    let rem = data.len() % 16;
    let plain_blocks = if rem == 0 { full } else { full - 1 };
    for i in 0..plain_blocks {
        let mut b: [u8; 16] = out[i * 16..i * 16 + 16].try_into().expect("whole block");
        one(&mut b, &t);
        out[i * 16..i * 16 + 16].copy_from_slice(&b);
        t = next(&t);
    }
    if rem > 0 {
        let base = plain_blocks * 16;
        let (t1, t2) = if encrypt { (t, next(&t)) } else { (next(&t), t) };
        let mut cc: [u8; 16] = data[base..base + 16].try_into().expect("whole block");
        one(&mut cc, &t1);
        let tail = data[base + 16..].to_vec();
        let mut cp = cc;
        cp[..rem].copy_from_slice(&tail);
        one(&mut cp, &t2);
        out[base..base + 16].copy_from_slice(&cp);
        out[base + 16..].copy_from_slice(&cc[..rem]);
    }
    Ok(out)
}

/// RFC 3394 key wrap of `data` (a multiple of 8 bytes) with the 8-byte `iv`.
fn kw_wrap(block: &Block, iv: [u8; 8], data: &[u8]) -> Vec<u8> {
    let n = data.len() / 8;
    let mut a = iv;
    let mut r: Vec<[u8; 8]> = data.chunks(8).map(|c| c.try_into().expect("8-byte chunk")).collect();
    for j in 0..6 {
        for (i, ri) in r.iter_mut().enumerate() {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&a);
            b[8..].copy_from_slice(ri);
            block.encrypt(&mut b);
            let t = (n * j + i + 1) as u64;
            a.copy_from_slice(&b[..8]);
            for (x, y) in a.iter_mut().zip(t.to_be_bytes()) {
                *x ^= y;
            }
            ri.copy_from_slice(&b[8..]);
        }
    }
    let mut out = a.to_vec();
    for ri in r {
        out.extend_from_slice(&ri);
    }
    out
}

/// RFC 3394 unwrap: `(A, plaintext)`.
fn kw_unwrap(block: &Block, data: &[u8]) -> ([u8; 8], Vec<u8>) {
    let n = data.len() / 8 - 1;
    let mut a: [u8; 8] = data[..8].try_into().expect("8 bytes");
    let mut r: Vec<[u8; 8]> = data[8..].chunks(8).map(|c| c.try_into().expect("8-byte chunk")).collect();
    for j in (0..6).rev() {
        for i in (0..n).rev() {
            let t = (n * j + i + 1) as u64;
            for (x, y) in a.iter_mut().zip(t.to_be_bytes()) {
                *x ^= y;
            }
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&a);
            b[8..].copy_from_slice(&r[i]);
            block.decrypt(&mut b);
            a.copy_from_slice(&b[..8]);
            r[i].copy_from_slice(&b[8..]);
        }
    }
    (a, r.concat())
}

/// RFC 5649 key wrap with padding; `iv` is the 4-byte alternative IV prefix.
fn kwp(block: &Block, iv: [u8; 4], data: &[u8], wrap: bool) -> CResult<Vec<u8>> {
    if wrap {
        if data.is_empty() {
            return Err(CipherError::update_state());
        }
        let mut aiv = [0u8; 8];
        aiv[..4].copy_from_slice(&iv);
        aiv[4..].copy_from_slice(&(data.len() as u32).to_be_bytes());
        let mut padded = data.to_vec();
        padded.resize(data.len().div_ceil(8) * 8, 0);
        if padded.len() == 8 {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&aiv);
            b[8..].copy_from_slice(&padded);
            block.encrypt(&mut b);
            return Ok(b.to_vec());
        }
        return Ok(kw_wrap(block, aiv, &padded));
    }
    if data.len() < 16 || !data.len().is_multiple_of(8) {
        return Err(CipherError::update_state());
    }
    let (a, p) = if data.len() == 16 {
        let mut b: [u8; 16] = data.try_into().expect("16 bytes");
        block.decrypt(&mut b);
        (b[..8].try_into().expect("8 bytes"), b[8..].to_vec())
    } else {
        kw_unwrap(block, data)
    };
    let mli = u32::from_be_bytes([a[4], a[5], a[6], a[7]]) as usize;
    if a[..4] != iv || mli > p.len() || p.len() - mli >= 8 || p[mli..].iter().any(|&b| b != 0) {
        return Err(CipherError::update_state());
    }
    Ok(p[..mli].to_vec())
}

const CMS_3DES_IV: [u8; 8] = [0x4a, 0xdd, 0xa2, 0x2c, 0x79, 0xe8, 0x21, 0x05];

fn cbc_raw(block: &Block, iv: &[u8], data: &mut [u8], encrypt: bool) {
    let bs = block.size();
    let mut prev = iv.to_vec();
    for chunk in data.chunks_mut(bs) {
        if encrypt {
            chunk.iter_mut().zip(&prev).for_each(|(b, p)| *b ^= p);
            block.encrypt(chunk);
            prev.copy_from_slice(chunk);
        } else {
            let saved = chunk.to_vec();
            block.decrypt(chunk);
            chunk.iter_mut().zip(&prev).for_each(|(b, p)| *b ^= p);
            prev = saved;
        }
    }
}

/// CMS Triple-DES key wrap (RFC 3217).
fn des3_wrap(block: &Block, data: &[u8], wrap: bool) -> CResult<Vec<u8>> {
    use sha1::Digest;
    if wrap {
        if data.is_empty() || !data.len().is_multiple_of(8) {
            return Err(CipherError::update_state());
        }
        let mut cekicv = data.to_vec();
        cekicv.extend_from_slice(&sha1::Sha1::digest(data)[..8]);
        let mut iv = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut iv);
        cbc_raw(block, &iv, &mut cekicv, true);
        let mut temp = iv.to_vec();
        temp.extend_from_slice(&cekicv);
        temp.reverse();
        cbc_raw(block, &CMS_3DES_IV, &mut temp, true);
        return Ok(temp);
    }
    if data.len() < 24 || !data.len().is_multiple_of(8) {
        return Err(CipherError::update_state());
    }
    let mut temp = data.to_vec();
    cbc_raw(block, &CMS_3DES_IV, &mut temp, false);
    temp.reverse();
    let (iv, rest) = temp.split_at(8);
    let mut cekicv = rest.to_vec();
    cbc_raw(block, iv, &mut cekicv, false);
    let (cek, icv) = cekicv.split_at(cekicv.len() - 8);
    if sha1::Sha1::digest(cek)[..8] != *icv {
        return Err(CipherError::update_state());
    }
    Ok(cek.to_vec())
}

// A cipher state lives boxed in its JS handle; variant sizes do not matter.
#[allow(clippy::large_enum_variant)]
enum Mode {
    Cbc { block: Block, iv: Vec<u8> },
    Ecb { block: Block },
    /// CTR with a 128-bit big-endian counter (OpenSSL), or GCM's 32-bit counter.
    Ctr { block: Block, counter: [u8; 16], ks: [u8; 16], used: usize, inc32: bool },
    /// CFB with full-block feedback: `reg` holds the previous ciphertext block.
    Cfb { block: Block, reg: Vec<u8>, ks: Vec<u8>, used: usize },
    Cfb8 { block: Block, reg: Vec<u8> },
    Cfb1 { block: Block, reg: Vec<u8> },
    Ofb { block: Block, reg: Vec<u8>, used: usize },
    ChaCha { cipher: Box<chacha20::ChaCha20> },
    Ccm(Box<CcmState>),
    Ocb(Box<OcbState>),
    Xts { k1: Block, k2: Block, iv: Vec<u8> },
    Wrap { block: Block, iv: [u8; 8] },
    WrapPad { block: Block, iv: [u8; 4] },
    Des3Wrap { block: Block },
    HmacStitched,
    /// OpenSSL refuses XTS encryption with identical key halves once data is processed.
    XtsDupKeys,
}

pub(crate) struct CipherState {
    encrypt: bool,
    mode: Mode,
    padding: bool,
    buf: Vec<u8>,
    aead: Option<AeadState>,
    /// AEAD tag: computed (encryption) or expected (decryption).
    tag: Option<Vec<u8>>,
    expected_tag: Option<Vec<u8>>,
    finalized: bool,
    updated: bool,
}

impl CipherState {
    pub(crate) fn new(name: &str, key: &[u8], iv: Option<&[u8]>, encrypt: bool, auth_tag_len: Option<usize>) -> CResult<Self> {
        let Some(spec) = lookup(name) else {
            if legacy_cipher(name) {
                return Err(CipherError::new("Error", "error:0308010C:digital envelope routines::unsupported", Some("ERR_OSSL_EVP_UNSUPPORTED")));
            }
            return Err(CipherError::new("Error", "Unknown cipher", Some("ERR_CRYPTO_UNKNOWN_CIPHER")));
        };
        if key.len() != spec.key {
            return Err(CipherError::new("RangeError", "Invalid key length", Some("ERR_CRYPTO_INVALID_KEYLEN")));
        }
        let bad_iv = || CipherError::new("TypeError", "Invalid initialization vector", Some("ERR_CRYPTO_INVALID_IV"));
        let iv_given = iv.is_some();
        let iv = iv.unwrap_or(&[]);
        let tag_required = |ok: &dyn Fn(usize) -> bool| -> CResult<usize> {
            match auth_tag_len {
                None => Err(CipherError::new("TypeError", &format!("authTagLength required for {name}"), Some("ERR_CRYPTO_INVALID_AUTH_TAG"))),
                Some(n) if !ok(n) => Err(CipherError::bad_tag_len(n)),
                Some(n) => Ok(n),
            }
        };
        let fixed_iv = |len: usize| -> CResult<()> { if iv.len() != len || (len > 0 && !iv_given) { Err(bad_iv()) } else { Ok(()) } };
        let mut aead = None;
        let block = || Block::new(spec.alg, key);
        let mode = match spec.mode {
            M::Cbc => {
                fixed_iv(spec.iv)?;
                Mode::Cbc { block: block(), iv: iv.to_vec() }
            }
            M::Ecb => {
                if !iv.is_empty() {
                    return Err(bad_iv());
                }
                Mode::Ecb { block: block() }
            }
            M::Ctr => {
                fixed_iv(16)?;
                let mut counter = [0u8; 16];
                counter.copy_from_slice(iv);
                Mode::Ctr { block: block(), counter, ks: [0; 16], used: 16, inc32: false }
            }
            M::Cfb => {
                fixed_iv(spec.iv)?;
                Mode::Cfb { block: block(), reg: iv.to_vec(), ks: vec![0; spec.iv], used: spec.iv }
            }
            M::Cfb8 => {
                fixed_iv(spec.iv)?;
                Mode::Cfb8 { block: block(), reg: iv.to_vec() }
            }
            M::Cfb1 => {
                fixed_iv(spec.iv)?;
                Mode::Cfb1 { block: block(), reg: iv.to_vec() }
            }
            M::Ofb => {
                fixed_iv(spec.iv)?;
                Mode::Ofb { block: block(), reg: iv.to_vec(), used: spec.iv }
            }
            M::Gcm => {
                if iv.is_empty() {
                    return Err(bad_iv());
                }
                let tag_len = auth_tag_len.unwrap_or(16);
                if !(4..=16).contains(&tag_len) || (tag_len < 12 && tag_len != 4 && tag_len != 8) {
                    return Err(CipherError::bad_tag_len(tag_len));
                }
                let block = block();
                let mut h = [0u8; 16];
                block.encrypt(&mut h);
                let mut j0 = [0u8; 16];
                if iv.len() == 12 {
                    j0[..12].copy_from_slice(iv);
                    j0[15] = 1;
                } else {
                    let mut g = ghash::GHash::new(GenericArray::from_slice(&h));
                    g.update_padded(iv);
                    let mut lens = [0u8; 16];
                    lens[8..].copy_from_slice(&((iv.len() as u64) * 8).to_be_bytes());
                    g.update(&[*GenericArray::from_slice(&lens)]);
                    j0.copy_from_slice(&g.finalize());
                }
                let mut mask = j0;
                block.encrypt(&mut mask);
                let mut counter = j0;
                inc32(&mut counter);
                aead = Some(AeadState {
                    mac: Mac::Ghash(Box::new(ghash::GHash::new(GenericArray::from_slice(&h)))),
                    aad_len: 0,
                    ct_len: 0,
                    pending: Vec::new(),
                    in_ct: false,
                    tag_mask: mask,
                    tag_len,
                });
                Mode::Ctr { block, counter, ks: [0; 16], used: 16, inc32: true }
            }
            M::Ccm => {
                let tag_len = tag_required(&|n| (4..=16).contains(&n) && n % 2 == 0)?;
                if !(7..=13).contains(&iv.len()) {
                    return Err(bad_iv());
                }
                Mode::Ccm(Box::new(CcmState { block: block(), nonce: iv.to_vec(), tag_len, aad: Vec::new(), plaintext_len: None, done_update: false, failed: false }))
            }
            M::Ocb => {
                let tag_len = tag_required(&|n| (1..=16).contains(&n))?;
                if !(1..=15).contains(&iv.len()) {
                    return Err(bad_iv());
                }
                Mode::Ocb(Box::new(OcbState::new(block(), iv, tag_len)))
            }
            M::Xts => {
                fixed_iv(16)?;
                let half = key.len() / 2;
                Mode::Xts { k1: Block::aes(&key[..half]), k2: Block::aes(&key[half..]), iv: iv.to_vec() }
            }
            M::Wrap => {
                fixed_iv(8)?;
                Mode::Wrap { block: block(), iv: iv.try_into().expect("iv length checked") }
            }
            M::WrapPad => {
                fixed_iv(4)?;
                Mode::WrapPad { block: block(), iv: iv.try_into().expect("iv length checked") }
            }
            M::Des3Wrap => {
                if !iv.is_empty() {
                    return Err(bad_iv());
                }
                Mode::Des3Wrap { block: block() }
            }
            M::HmacStitched => {
                fixed_iv(16)?;
                Mode::HmacStitched
            }
            M::ChaCha => {
                fixed_iv(16)?;
                let counter = u32::from_le_bytes([iv[0], iv[1], iv[2], iv[3]]);
                let mut c = chacha20::ChaCha20::new(GenericArray::from_slice(key), GenericArray::from_slice(&iv[4..16]));
                c.seek(counter as u64 * 64);
                Mode::ChaCha { cipher: Box::new(c) }
            }
            M::ChaChaPoly => {
                // RFC 8439
                if iv.len() != 12 {
                    return Err(bad_iv());
                }
                let tag_len = auth_tag_len.unwrap_or(16);
                if !(1..=16).contains(&tag_len) {
                    return Err(CipherError::bad_tag_len(tag_len));
                }
                let mut c = chacha20::ChaCha20::new(GenericArray::from_slice(key), GenericArray::from_slice(iv));
                let mut poly_key = [0u8; 32];
                c.apply_keystream(&mut poly_key);
                c.seek(64u64);
                aead = Some(AeadState {
                    mac: Mac::Poly(Box::new(poly1305::Poly1305::new(GenericArray::from_slice(&poly_key)))),
                    aad_len: 0,
                    ct_len: 0,
                    pending: Vec::new(),
                    in_ct: false,
                    tag_mask: [0; 16],
                    tag_len,
                });
                Mode::ChaCha { cipher: Box::new(c) }
            }
        };
        if matches!(mode, Mode::Xts { .. }) && encrypt && key[..key.len() / 2] == key[key.len() / 2..] {
            return Ok(Self::with_mode(encrypt, Mode::XtsDupKeys, None));
        }
        Ok(Self::with_mode(encrypt, mode, aead))
    }

    fn with_mode(encrypt: bool, mode: Mode, aead: Option<AeadState>) -> Self {
        Self { encrypt, mode, padding: true, buf: Vec::new(), aead, tag: None, expected_tag: None, finalized: false, updated: false }
    }

    fn is_block_mode(&self) -> bool {
        matches!(self.mode, Mode::Cbc { .. } | Mode::Ecb { .. })
    }

    fn block_size(&self) -> usize {
        match &self.mode {
            Mode::Cbc { block, .. } | Mode::Ecb { block } => block.size(),
            _ => 1,
        }
    }

    fn stream(&mut self, data: &[u8]) -> Vec<u8> {
        let encrypt = self.encrypt;
        let mut out = data.to_vec();
        match &mut self.mode {
            Mode::Ctr { block, counter, ks, used, inc32: is32 } => {
                for b in out.iter_mut() {
                    if *used == 16 {
                        *ks = *counter;
                        block.encrypt(ks);
                        if *is32 {
                            inc32(counter);
                        } else {
                            inc128(counter);
                        }
                        *used = 0;
                    }
                    *b ^= ks[*used];
                    *used += 1;
                }
            }
            Mode::Cfb { block, reg, ks, used } => {
                for b in out.iter_mut() {
                    if *used == reg.len() {
                        ks.copy_from_slice(reg);
                        block.encrypt(ks);
                        *used = 0;
                    }
                    let c = if encrypt { *b ^ ks[*used] } else { *b };
                    *b ^= ks[*used];
                    reg[*used] = c;
                    *used += 1;
                }
            }
            Mode::Cfb8 { block, reg } => {
                for b in out.iter_mut() {
                    let ks = block.encrypted(reg);
                    let c = if encrypt { *b ^ ks[0] } else { *b };
                    *b ^= ks[0];
                    reg.rotate_left(1);
                    let last = reg.len() - 1;
                    reg[last] = c;
                }
            }
            Mode::Cfb1 { block, reg } => {
                for b in out.iter_mut() {
                    let mut byte = 0u8;
                    for bit in (0..8).rev() {
                        let ks = block.encrypted(reg);
                        let inb = (*b >> bit) & 1;
                        let outb = inb ^ (ks[0] >> 7);
                        let c = if encrypt { outb } else { inb };
                        byte |= outb << bit;
                        // Shift the register left by one bit, appending the ciphertext bit.
                        for i in 0..reg.len() {
                            let next = if i + 1 < reg.len() { reg[i + 1] >> 7 } else { c };
                            reg[i] = (reg[i] << 1) | next;
                        }
                    }
                    *b = byte;
                }
            }
            Mode::Ofb { block, reg, used } => {
                for b in out.iter_mut() {
                    if *used == reg.len() {
                        block.encrypt(reg);
                        *used = 0;
                    }
                    *b ^= reg[*used];
                    *used += 1;
                }
            }
            Mode::ChaCha { cipher } => cipher.apply_keystream(&mut out),
            _ => unreachable!(),
        }
        out
    }

    fn blocks(&mut self, n: usize) -> Vec<u8> {
        let mut data: Vec<u8> = self.buf.drain(..n).collect();
        let encrypt = self.encrypt;
        match &mut self.mode {
            Mode::Cbc { block, iv } => {
                if !data.is_empty() {
                    let bs = block.size();
                    let last_ct = if encrypt { None } else { Some(data[data.len() - bs..].to_vec()) };
                    cbc_raw(block, iv, &mut data, encrypt);
                    *iv = last_ct.unwrap_or_else(|| data[data.len() - bs..].to_vec());
                }
            }
            Mode::Ecb { block } => {
                let bs = block.size();
                for chunk in data.chunks_mut(bs) {
                    if encrypt { block.encrypt(chunk) } else { block.decrypt(chunk) }
                }
            }
            _ => unreachable!(),
        }
        data
    }

    pub(crate) fn update(&mut self, data: &[u8]) -> CResult<Vec<u8>> {
        if self.finalized {
            return Err(CipherError::new("Error", "Unsupported state", Some("ERR_CRYPTO_INVALID_STATE")));
        }
        self.updated = true;
        let encrypt = self.encrypt;
        match &mut self.mode {
            Mode::HmacStitched => return Err(CipherError::update_state()),
            Mode::XtsDupKeys => {
                return Err(CipherError::new("Error", "error:1C800095:Provider routines::xts duplicated keys", Some("ERR_OSSL_XTS_DUPLICATED_KEYS")));
            }
            Mode::Xts { k1, k2, iv } => return xts(k1, k2, iv, data, encrypt),
            Mode::Wrap { block, iv } => {
                let ok = if encrypt { data.len() >= 16 } else { data.len() >= 24 };
                if !ok || !data.len().is_multiple_of(8) {
                    return Err(CipherError::update_state());
                }
                if encrypt {
                    return Ok(kw_wrap(block, *iv, data));
                }
                let (a, p) = kw_unwrap(block, data);
                return if a == *iv { Ok(p) } else { Err(CipherError::update_state()) };
            }
            Mode::WrapPad { block, iv } => return kwp(block, *iv, data, encrypt),
            Mode::Des3Wrap { block } => return des3_wrap(block, data, encrypt),
            Mode::Ccm(c) => {
                if c.done_update || c.plaintext_len.is_some_and(|n| n != data.len()) {
                    return Err(CipherError::update_state());
                }
                c.done_update = true;
                if encrypt {
                    let mac = c.mac(data);
                    let mut out = data.to_vec();
                    c.ctr(&mut out);
                    self.tag = Some(c.tag(mac));
                    return Ok(out);
                }
                let mut out = data.to_vec();
                c.ctr(&mut out);
                let expected = self.expected_tag.clone();
                let good = expected.is_some_and(|t| t == c.tag(c.mac(&out)));
                if !good {
                    c.failed = true;
                    return Ok(Vec::new());
                }
                return Ok(out);
            }
            Mode::Ocb(o) => {
                self.buf.extend_from_slice(data);
                let n = self.buf.len() / 16 * 16;
                let chunk: Vec<u8> = self.buf.drain(..n).collect();
                return Ok(o.process(&chunk, encrypt));
            }
            _ => {}
        }
        if !self.is_block_mode() {
            if !self.encrypt && let Some(a) = &mut self.aead {
                a.add_ct(data);
            }
            let out = self.stream(data);
            if self.encrypt && let Some(a) = &mut self.aead {
                a.add_ct(&out);
            }
            return Ok(out);
        }
        let bs = self.block_size();
        self.buf.extend_from_slice(data);
        let mut n = self.buf.len() / bs * bs;
        // Decrypting with padding keeps the last full block for `final` (it holds the padding).
        if !self.encrypt && self.padding && n == self.buf.len() && n > 0 {
            n -= bs;
        }
        Ok(if n > 0 { self.blocks(n) } else { Vec::new() })
    }

    pub(crate) fn finalize(&mut self) -> CResult<Vec<u8>> {
        if self.finalized {
            return Err(CipherError::new("Error", "Unsupported state", Some("ERR_CRYPTO_INVALID_STATE")));
        }
        self.finalized = true;
        let encrypt = self.encrypt;
        match &mut self.mode {
            Mode::Ccm(c) => {
                if encrypt {
                    if !c.done_update {
                        return Err(CipherError::new("Error", "error:1C800077:Provider routines::tag not set", Some("ERR_OSSL_TAG_NOT_SET")));
                    }
                    return Ok(Vec::new());
                }
                return if c.failed || !c.done_update { Err(CipherError::auth()) } else { Ok(Vec::new()) };
            }
            Mode::Ocb(o) => {
                let rest = std::mem::take(&mut self.buf);
                let out = o.last(&rest, encrypt);
                let tag = o.tag();
                if encrypt {
                    self.tag = Some(tag);
                } else if self.expected_tag.as_ref().is_none_or(|t| !ct_eq(t, &tag[..t.len().min(tag.len())]) || t.len() != tag.len()) {
                    return Err(CipherError::auth());
                }
                return Ok(out);
            }
            Mode::Xts { .. } | Mode::Wrap { .. } | Mode::WrapPad { .. } | Mode::Des3Wrap { .. } | Mode::HmacStitched | Mode::XtsDupKeys => return Ok(Vec::new()),
            _ => {}
        }
        if !self.is_block_mode() {
            if let Some(a) = &mut self.aead {
                let tag = a.finish();
                if self.encrypt {
                    self.tag = Some(tag[..a.tag_len].to_vec());
                } else {
                    let Some(expected) = &self.expected_tag else { return Err(CipherError::auth()) };
                    if !ct_eq(&tag[..expected.len()], expected) {
                        return Err(CipherError::auth());
                    }
                }
            }
            return Ok(Vec::new());
        }
        let bs = self.block_size();
        if self.encrypt {
            if self.padding {
                let pad = bs - self.buf.len() % bs;
                self.buf.extend(std::iter::repeat_n(pad as u8, pad));
            } else if !self.buf.len().is_multiple_of(bs) {
                return Err(CipherError::wrong_final());
            }
            let n = self.buf.len();
            return Ok(self.blocks(n));
        }
        if !self.buf.len().is_multiple_of(bs) {
            return Err(CipherError::wrong_final());
        }
        if !self.padding {
            let n = self.buf.len();
            return Ok(self.blocks(n));
        }
        if self.buf.len() != bs {
            return Err(CipherError::wrong_final());
        }
        let mut last = self.blocks(bs);
        let pad = *last.last().unwrap_or(&0) as usize;
        if pad == 0 || pad > bs || !last[bs - pad..].iter().all(|&b| b as usize == pad) {
            return Err(CipherError::bad_decrypt());
        }
        last.truncate(bs - pad);
        Ok(last)
    }

    /// `setAAD(aad, { plaintextLength })`.
    pub(crate) fn set_aad(&mut self, aad: &[u8], plaintext_len: Option<usize>) -> CResult<()> {
        match &mut self.mode {
            Mode::Ccm(c) => {
                if c.done_update {
                    return Err(CipherError::new("Error", "Unsupported state", None));
                }
                c.aad.extend_from_slice(aad);
                c.plaintext_len = plaintext_len;
                return Ok(());
            }
            Mode::Ocb(o) => {
                if self.updated {
                    return Err(CipherError::new("Error", "Unsupported state", None));
                }
                o.aad.extend_from_slice(aad);
                return Ok(());
            }
            _ => {}
        }
        match &mut self.aead {
            Some(a) => a.add_aad(aad),
            None => Err(CipherError::new("Error", "Unsupported state", None)),
        }
    }

    fn is_aead(&self) -> bool {
        self.aead.is_some() || matches!(self.mode, Mode::Ccm(_) | Mode::Ocb(_))
    }

    pub(crate) fn auth_tag(&self) -> CResult<Vec<u8>> {
        match &self.tag {
            Some(t) if self.encrypt && self.finalized => Ok(t.clone()),
            _ => Err(CipherError::new("Error", "Invalid state for operation getAuthTag", Some("ERR_CRYPTO_INVALID_STATE"))),
        }
    }

    pub(crate) fn set_auth_tag(&mut self, tag: &[u8]) -> CResult<()> {
        if self.encrypt || !self.is_aead() || self.finalized {
            return Err(CipherError::new("Error", "Invalid state for operation setAuthTag", Some("ERR_CRYPTO_INVALID_STATE")));
        }
        let ok = match (&self.mode, &self.aead) {
            (Mode::Ccm(c), _) => tag.len() == c.tag_len,
            (Mode::Ocb(o), _) => tag.len() == o.tag_len,
            (_, Some(a)) if matches!(a.mac, Mac::Ghash(_)) => (4..=16).contains(&tag.len()) && (tag.len() >= 12 || tag.len() == 4 || tag.len() == 8),
            _ => (1..=16).contains(&tag.len()),
        };
        if !ok {
            return Err(CipherError::bad_tag_len(tag.len()));
        }
        self.expected_tag = Some(tag.to_vec());
        Ok(())
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn inc32(c: &mut [u8; 16]) {
    let n = u32::from_be_bytes([c[12], c[13], c[14], c[15]]).wrapping_add(1);
    c[12..].copy_from_slice(&n.to_be_bytes());
}

fn inc128(c: &mut [u8; 16]) {
    let n = u128::from_be_bytes(*c).wrapping_add(1);
    *c = n.to_be_bytes();
}

/// One-shot encryption/decryption (WebCrypto, tests).
pub(crate) fn one_shot(name: &str, key: &[u8], iv: Option<&[u8]>, encrypt: bool, data: &[u8]) -> CResult<Vec<u8>> {
    let mut c = CipherState::new(name, key, iv, encrypt, None)?;
    let mut out = c.update(data)?;
    out.extend(c.finalize()?);
    Ok(out)
}

/// AES key wrap (RFC 3394), for WebCrypto `AES-KW`.
pub(crate) fn aes_kw(key: &[u8], data: &[u8], wrap: bool) -> CResult<Vec<u8>> {
    if ![16, 24, 32].contains(&key.len()) {
        return Err(CipherError::new("Error", "Invalid key length", None));
    }
    let block = Block::aes(key);
    let iv = [0xa6u8; 8];
    if wrap {
        if !data.len().is_multiple_of(8) || data.len() < 16 {
            return Err(CipherError::new("Error", "Invalid data length", None));
        }
        Ok(kw_wrap(&block, iv, data))
    } else {
        if !data.len().is_multiple_of(8) || data.len() < 24 {
            return Err(CipherError::new("Error", "Invalid data length", None));
        }
        let (a, p) = kw_unwrap(&block, data);
        if a != iv {
            return Err(CipherError::new("Error", "Integrity check failed", None));
        }
        Ok(p)
    }
}

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

fn opt_bytes(v: &JsValue, ctx: &mut Context) -> JsResult<Option<Vec<u8>>> {
    if v.is_null_or_undefined() { Ok(None) } else { bytes_arg(v, ctx).map(Some) }
}

/// `ops.cipherNew(name, key, iv|null, encrypt, authTagLength?) -> handle`
fn op_cipher_new(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_string(args, 0, ctx)?;
    let key = bytes_arg(args.get_or_undefined(1), ctx)?;
    let iv = opt_bytes(args.get_or_undefined(2), ctx)?;
    let encrypt = args.get_or_undefined(3).to_boolean();
    let tag_len = args.get_or_undefined(4);
    let tag_len = if tag_len.is_null_or_undefined() { None } else { Some(tag_len.to_number(ctx)? as usize) };
    match CipherState::new(&name, &key, iv.as_deref(), encrypt, tag_len) {
        Ok(s) => Ok(native_object(s)),
        Err(e) => Err(e.throw(ctx)),
    }
}

fn op_cipher_update(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let data = bytes_arg(args.get_or_undefined(1), ctx)?;
    match with_native::<CipherState, _>(args.get_or_undefined(0), |c| c.update(&data))? {
        Ok(out) => bytes_to_js(&out, ctx),
        Err(e) => Err(e.throw(ctx)),
    }
}

fn op_cipher_final(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    match with_native::<CipherState, _>(args.get_or_undefined(0), |c| c.finalize())? {
        Ok(out) => bytes_to_js(&out, ctx),
        Err(e) => Err(e.throw(ctx)),
    }
}

fn op_cipher_ctl(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let what = arg_string(args, 1, ctx)?;
    let arg = args.get_or_undefined(2).clone();
    let bytes = if what == "setAAD" || what == "setAuthTag" { bytes_arg(&arg, ctx)? } else { Vec::new() };
    let plaintext_len = args.get_or_undefined(3);
    let plaintext_len = if plaintext_len.is_null_or_undefined() { None } else { Some(plaintext_len.to_number(ctx)? as usize) };
    let r = with_native::<CipherState, _>(args.get_or_undefined(0), |c| match what.as_str() {
        "setAAD" => c.set_aad(&bytes, plaintext_len).map(|_| None),
        "setAuthTag" => c.set_auth_tag(&bytes).map(|_| None),
        "getAuthTag" => c.auth_tag().map(Some),
        "setAutoPadding" => {
            c.padding = arg.to_boolean();
            Ok(None)
        }
        _ => Ok(None),
    })?;
    match r {
        Ok(Some(b)) => bytes_to_js(&b, ctx),
        Ok(None) => Ok(JsValue::undefined()),
        Err(e) => Err(e.throw(ctx)),
    }
}

/// `ops.cipherInfo(name) -> { name, nid, keyLength, ivLength, blockSize, mode } | undefined`
fn op_cipher_info(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_string(args, 0, ctx)?;
    match lookup(&name) {
        Some(s) => super::object(
            ctx,
            vec![
                ("name", js_str(s.canonical)),
                ("nid", JsValue::from(s.nid)),
                ("keyLength", JsValue::from(s.key as u32)),
                ("ivLength", JsValue::from(s.iv as u32)),
                ("blockSize", JsValue::from(s.bs as u32)),
                ("mode", js_str(s.info_mode)),
            ],
        ),
        None => Ok(JsValue::undefined()),
    }
}

fn op_ciphers(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let items = cipher_names().into_iter().map(js_str).collect();
    Ok(super::array(ctx, items))
}

/// `ops.aesKw(key, data, wrap) -> bytes`
fn op_aes_kw(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let key = bytes_arg(args.get_or_undefined(0), ctx)?;
    let data = bytes_arg(args.get_or_undefined(1), ctx)?;
    match aes_kw(&key, &data, args.get_or_undefined(2).to_boolean()) {
        Ok(out) => bytes_to_js(&out, ctx),
        Err(e) => Err(e.throw(ctx)),
    }
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("cipherNew", 5, op_cipher_new),
    ("cipherUpdate", 2, op_cipher_update),
    ("cipherFinal", 1, op_cipher_final),
    ("cipherCtl", 4, op_cipher_ctl),
    ("cipherInfo", 1, op_cipher_info),
    ("ciphers", 0, op_ciphers),
    ("aesKw", 3, op_aes_kw),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn matches_node_vectors() {
        let key = b"0123456789abcdef";
        let iv = b"fedcba9876543210";
        // node -e 'c=require("crypto");x=c.createCipheriv(N,K,IV);console.log(Buffer.concat([x.update("hello world, this is a test"),x.final()]).toString("hex"))'
        assert_eq!(hex(&one_shot("aes-128-cbc", key, Some(iv), true, b"hello world, this is a test").unwrap()), "48a64c77eda317c2ece0b3eb1013593a9ddabef656d84916dd63e7bd9f8a76f4");
        assert_eq!(hex(&one_shot("aes-128-ctr", key, Some(iv), true, b"hello world, this is a test").unwrap()), "632eba1d99500866ca54a7faea6802543145854a8fc6adfb838b25");
        let ct = one_shot("aes-128-cbc", key, Some(iv), true, b"x").unwrap();
        assert_eq!(one_shot("aes-128-cbc", key, Some(iv), false, &ct).unwrap(), b"x");
    }

    #[test]
    fn gcm_matches_nist_vector() {
        // NIST GCM test case 4 (AES-128, 60-byte plaintext, 20-byte AAD).
        let key = unhex("feffe9928665731c6d6a8f9467308308");
        let iv = unhex("cafebabefacedbaddecaf888");
        let pt = unhex("d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39");
        let aad = unhex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        let mut c = CipherState::new("aes-128-gcm", &key, Some(&iv), true, None).unwrap();
        c.set_aad(&aad, None).unwrap();
        let mut ct = Vec::new();
        for chunk in pt.chunks(7) {
            ct.extend(c.update(chunk).unwrap());
        }
        ct.extend(c.finalize().unwrap());
        assert_eq!(hex(&ct), "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091");
        assert_eq!(hex(&c.auth_tag().unwrap()), "5bc94fbc3221a5db94fae95ae7121a47");
        let mut d = CipherState::new("aes-128-gcm", &key, Some(&iv), false, None).unwrap();
        d.set_aad(&aad, None).unwrap();
        d.set_auth_tag(&c.auth_tag().unwrap()).unwrap();
        assert_eq!(d.update(&ct).unwrap(), pt);
        d.finalize().unwrap();
        let mut bad = CipherState::new("aes-128-gcm", &key, Some(&iv), false, None).unwrap();
        bad.set_auth_tag(&[0; 16]).unwrap();
        bad.update(&ct).unwrap();
        assert_eq!(bad.finalize().unwrap_err().message, "Unsupported state or unable to authenticate data");
    }

    #[test]
    fn chacha20_poly1305_matches_rfc8439() {
        let key = unhex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
        let iv = unhex("070000004041424344454647");
        let aad = unhex("50515253c0c1c2c3c4c5c6c7");
        let pt = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let mut c = CipherState::new("chacha20-poly1305", &key, Some(&iv), true, None).unwrap();
        c.set_aad(&aad, None).unwrap();
        let ct = c.update(pt).unwrap();
        c.finalize().unwrap();
        assert_eq!(hex(&ct[..16]), "d31a8d34648e60db7b86afbc53ef7ec2");
        assert_eq!(hex(&c.auth_tag().unwrap()), "1ae10b594f09e26a7e902ecbd0600691");
    }

    #[test]
    fn errors_match_node() {
        let e = CipherState::new("foo", &[0; 16], Some(&[0; 16]), true, None).err().unwrap();
        assert_eq!((e.message.as_str(), e.code), ("Unknown cipher", Some("ERR_CRYPTO_UNKNOWN_CIPHER")));
        let e = CipherState::new("aes-128-cbc", &[0; 15], Some(&[0; 16]), true, None).err().unwrap();
        assert_eq!((e.kind, e.message.as_str()), ("RangeError", "Invalid key length"));
        let e = CipherState::new("aes-128-cbc", &[0; 16], Some(&[0; 15]), true, None).err().unwrap();
        assert_eq!((e.kind, e.message.as_str()), ("TypeError", "Invalid initialization vector"));
        let e = one_shot("aes-128-cbc", &[0; 16], Some(&[0; 16]), false, &[0; 16]).unwrap_err();
        assert_eq!(e.code, Some("ERR_OSSL_BAD_DECRYPT"));
        let e = one_shot("aes-128-cbc", &[0; 16], Some(&[0; 16]), false, &[0; 15]).unwrap_err();
        assert_eq!(e.code, Some("ERR_OSSL_WRONG_FINAL_BLOCK_LENGTH"));
    }

    #[test]
    fn aes_kw_rfc3394() {
        let kek = unhex("000102030405060708090A0B0C0D0E0F");
        let data = unhex("00112233445566778899AABBCCDDEEFF");
        let wrapped = aes_kw(&kek, &data, true).unwrap();
        assert_eq!(hex(&wrapped), "1fa68b0a8112b447aef34bd8fb5a7b829d3e862371d2cfe5");
        assert_eq!(aes_kw(&kek, &wrapped, false).unwrap(), data);
    }
}
