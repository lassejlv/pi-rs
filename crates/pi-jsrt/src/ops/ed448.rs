//! Ed448 (RFC 8032, pure EdDSA without context) and X448 (RFC 7748) on the Goldilocks curve
//! arithmetic of `ed448-goldilocks`.

use ed448_goldilocks::curve::edwards::CompressedEdwardsY;
use ed448_goldilocks::Scalar;
use ed448_goldilocks::curve::{ExtendedPoint, MontgomeryPoint};
use sha3::digest::{ExtendableOutput, Update, XofReader};

/// `SHAKE256(dom4(0, "") || parts...)` with 114 bytes of output.
fn shake(parts: &[&[u8]], dom: bool) -> [u8; 114] {
    let mut h = sha3::Shake256::default();
    if dom {
        h.update(b"SigEd448\x00\x00");
    }
    for p in parts {
        h.update(p);
    }
    let mut out = [0u8; 114];
    h.finalize_xof().read(&mut out);
    out
}

fn reduce(wide: &[u8; 114]) -> Scalar {
    Scalar::from_bytes_mod_order_wide(wide)
}

/// The secret scalar `s` and the nonce prefix of a 57-byte private key.
fn expand(seed: &[u8; 57]) -> (Scalar, [u8; 57]) {
    let h = shake(&[seed], false);
    let mut s = [0u8; 114];
    s[..57].copy_from_slice(&h[..57]);
    s[0] &= 0xfc;
    s[55] |= 0x80;
    s[56] = 0;
    let mut prefix = [0u8; 57];
    prefix.copy_from_slice(&h[57..]);
    (reduce(&s), prefix)
}

pub(crate) fn ed448_public(seed: &[u8; 57]) -> [u8; 57] {
    let (s, _) = expand(seed);
    ExtendedPoint::generator().scalar_mul(&s).compress().0
}

pub(crate) fn ed448_sign(seed: &[u8; 57], msg: &[u8]) -> [u8; 114] {
    let (s, prefix) = expand(seed);
    let a = ExtendedPoint::generator().scalar_mul(&s).compress().0;
    let r = reduce(&shake(&[&prefix, msg], true));
    let big_r = ExtendedPoint::generator().scalar_mul(&r).compress().0;
    let k = reduce(&shake(&[&big_r, &a, msg], true));
    let big_s = r + k * s;
    let mut sig = [0u8; 114];
    sig[..57].copy_from_slice(&big_r);
    sig[57..].copy_from_slice(&big_s.to_bytes_rfc_8032());
    sig
}

pub(crate) fn ed448_verify(public: &[u8; 57], msg: &[u8], sig: &[u8]) -> bool {
    let Ok(sig) = <&[u8; 114]>::try_from(sig) else { return false };
    let Some(a) = CompressedEdwardsY(*public).decompress() else { return false };
    let mut r_bytes = [0u8; 57];
    r_bytes.copy_from_slice(&sig[..57]);
    let Some(r) = CompressedEdwardsY(r_bytes).decompress() else { return false };
    let mut s_bytes = [0u8; 57];
    s_bytes.copy_from_slice(&sig[57..]);
    let Some(s) = Scalar::from_canonical_bytes(s_bytes) else { return false };
    let k = reduce(&shake(&[&r_bytes, public, msg], true));
    // [4][S]B == [4]R + [4][k]A
    let lhs = ExtendedPoint::generator().scalar_mul(&s).double().double();
    let rhs = r.add(&a.scalar_mul(&k)).double().double();
    lhs.compress().0 == rhs.compress().0
}

fn x448_scalar(k: &[u8; 56]) -> Scalar {
    let mut k = *k;
    k[0] &= 252;
    k[55] |= 128;
    Scalar::from_bytes(k)
}

pub(crate) fn x448_public(k: &[u8; 56]) -> [u8; 56] {
    (&MontgomeryPoint::generator() * &x448_scalar(k)).0
}

/// X448 shared secret; `None` for an all-zero result (a low-order peer key).
pub(crate) fn x448(k: &[u8; 56], peer: &[u8; 56]) -> Option<[u8; 56]> {
    let out = (&MontgomeryPoint(*peer) * &x448_scalar(k)).0;
    if out.iter().all(|&b| b == 0) { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn ed448_rfc8032_blank_message() {
        let seed: [u8; 57] = unhex(
            "6c82a562cb808d10d632be89c8513ebf6c929f34ddfa8c9f63c9960ef6e348a3528c8a3fcc2f044e39a3fc5b94492f8f032e7549a20098f95b",
        )
        .try_into()
        .unwrap();
        let public = ed448_public(&seed);
        assert_eq!(
            public.to_vec(),
            unhex("5fd7449b59b461fd2ce787ec616ad46a1da1342485a70e1f8a0ea75d80e96778edf124769b46c7061bd6783df1e50f6cd1fa1abeafe8256180")
        );
        let sig = ed448_sign(&seed, b"");
        assert_eq!(
            sig.to_vec(),
            unhex(
                "533a37f6bbe457251f023c0d88f976ae2dfb504a843e34d2074fd823d41a591f2b233f034f628281f2fd7a22ddd47d7828c59bd0a21bfd3980
                 ff0d2028d4b18a9df63e006c5d1c2d345b925d8dc00b4104852db99ac5c7cdda8530a113a0f4dbb61149f05a7363268c71d95808ff2e652600"
            )
        );
        assert!(ed448_verify(&public, b"", &sig));
        assert!(!ed448_verify(&public, b"x", &sig));
    }

    #[test]
    fn x448_rfc7748() {
        let a: [u8; 56] = unhex("9a8f4925d1519f5775cf46b04b5800d4ee9ee8bae8bc5565d498c28dd9c9baf574a9419744897391006382a6f127ab1d9ac2d8c0a598726b").try_into().unwrap();
        let b: [u8; 56] = unhex("1c306a7ac2a0e2e0990b294470cba339e6453772b075811d8fad0d1d6927c120bb5ee8972b0d3e21374c9c921b09d1b0366f10b65173992d").try_into().unwrap();
        let pa = x448_public(&a);
        assert_eq!(pa.to_vec(), unhex("9b08f7cc31b7e3e67d22d5aea121074a273bd2b83de09c63faa73d2c22c5d9bbc836647241d953d40c5b12da88120d53177f80e532c41fa0"));
        let shared = x448(&a, &x448_public(&b)).unwrap();
        assert_eq!(shared.to_vec(), unhex("07fff4181ac6cc95ec1c16a94a0f74d12da232ce40a77552281d282bb60c0b56fd2464c335543936521c24403085d59a449a5037514a879d"));
    }
}
