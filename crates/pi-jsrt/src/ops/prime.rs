//! Prime generation and testing for `crypto.generatePrime`/`checkPrime` and
//! `crypto.createDiffieHellman(bits, generator)`, following OpenSSL's `BN_generate_prime_ex`
//! (top bits set, `add`/`rem` congruences, safe primes) and `DH_check` (Node's `verifyError`).

use crate::js::{Context, JsArgs, JsResult, JsValue};
use num_bigint_dig::{BigUint, RandBigInt};
use num_traits::{One, ToPrimitive, Zero};

use super::bytes_arg;
use super::cipher::CipherError;
use super::crypto::job;
use crate::engine::{OpFn, arg_f64};

/// OpenSSL's Miller-Rabin round count for a candidate of `bits` (FIPS 186-4 table).
fn default_checks(bits: usize) -> usize {
    match bits {
        b if b >= 3747 => 3,
        b if b >= 1345 => 4,
        b if b >= 476 => 5,
        b if b >= 400 => 6,
        b if b >= 347 => 7,
        b if b >= 308 => 8,
        b if b >= 55 => 27,
        _ => 34,
    }
}

const SMALL_PRIMES: usize = 2048;

fn small_primes() -> &'static [u32] {
    static PRIMES: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    PRIMES.get_or_init(|| {
        let mut sieve = vec![true; SMALL_PRIMES * 12];
        let mut out = Vec::new();
        for i in 2..sieve.len() {
            if sieve[i] {
                out.push(i as u32);
                if out.len() == SMALL_PRIMES {
                    break;
                }
                let mut j = i * i;
                while j < sieve.len() {
                    sieve[j] = false;
                    j += i;
                }
            }
        }
        out
    })
}

/// Probabilistic primality (trial division, then Miller-Rabin + Lucas as in Baillie-PSW).
pub(crate) fn is_prime(n: &BigUint, checks: usize) -> bool {
    if n < &BigUint::from(2u32) {
        return false;
    }
    for &p in small_primes() {
        let p = BigUint::from(p);
        if n == &p {
            return true;
        }
        if (n % &p).is_zero() {
            return false;
        }
    }
    let checks = if checks == 0 { default_checks(n.bits()) } else { checks };
    num_bigint_dig::prime::probably_prime(n, checks)
}

/// `BN_generate_prime_ex(bits, safe, add, rem)`: random starts meeting OpenSSL's constraints
/// (top bits set, `p % add == rem`), stepped through an incremental small-prime sieve before
/// the probabilistic tests (for safe primes, `(p - 1) / 2` is sieved and tested too).
pub(crate) fn generate(bits: usize, safe: bool, add: Option<&BigUint>, rem: Option<&BigUint>) -> Result<BigUint, CipherError> {
    if bits < 2 || (bits == 2 && safe) {
        return Err(CipherError::new("Error", "error:01800076:bignum routines::bits too small", None));
    }
    let mut rng = rand::thread_rng();
    let top = BigUint::one() << (bits - 1);
    let primes = &small_primes()[1..];
    loop {
        let mut r = rng.gen_biguint(bits) | &top;
        let step = if let Some(add) = add {
            let rem = rem.cloned().unwrap_or_else(|| BigUint::from(if safe { 3u32 } else { 1u32 }));
            r = &r - (&r % add) + rem;
            add.clone()
        } else {
            if bits >= 2 {
                r |= &top >> 1usize;
            }
            r |= BigUint::one();
            if safe && bits > 2 {
                r |= BigUint::from(3u32);
                BigUint::from(4u32)
            } else {
                BigUint::from(2u32)
            }
        };
        if bits <= 16 {
            // Tiny sizes: plain retries (the sieve would reject the small primes themselves).
            if r.bits() <= bits && is_prime(&r, 0) && (!safe || is_prime(&(&r >> 1usize), 0)) {
                return Ok(r);
            }
            continue;
        }
        let mods: Vec<u64> = primes.iter().map(|&p| (&r % p).to_u64().unwrap_or(0)).collect();
        let deltas: Vec<u64> = primes.iter().map(|&p| (&step % p).to_u64().unwrap_or(0)).collect();
        let mut candidate = r;
        'step: for k in 0u64..(1 << 16) {
            if k > 0 {
                candidate += &step;
            }
            if candidate.bits() > bits {
                break;
            }
            for (i, &p) in primes.iter().enumerate() {
                let res = (mods[i] + k * deltas[i]) % p as u64;
                // p | candidate, or (safe) p | (candidate - 1) / 2.
                if res == 0 || (safe && res == 1) {
                    continue 'step;
                }
            }
            if !is_prime(&candidate, 0) {
                continue;
            }
            if safe && !is_prime(&(&candidate >> 1usize), 0) {
                continue;
            }
            return Ok(candidate);
        }
    }
}

// DH_check flags.
const DH_CHECK_P_NOT_PRIME: u32 = 0x01;
const DH_CHECK_P_NOT_SAFE_PRIME: u32 = 0x02;
const DH_NOT_SUITABLE_GENERATOR: u32 = 0x08;
const DH_MODULUS_TOO_SMALL: u32 = 0x80;
const DH_MODULUS_TOO_LARGE: u32 = 0x100;

/// OpenSSL 3's `DH_check` for parameters without a subgroup order (Node's `verifyError`).
pub(crate) fn dh_check(p: &BigUint, g: &BigUint) -> u32 {
    let mut flags = 0;
    if (p % 2u32).is_zero() {
        flags |= DH_CHECK_P_NOT_PRIME;
    }
    if g <= &BigUint::one() || p.is_zero() || g >= &(p - BigUint::one()) {
        flags |= DH_NOT_SUITABLE_GENERATOR;
    }
    if p.bits() < 512 {
        flags |= DH_MODULUS_TOO_SMALL;
    }
    if p.bits() > 10000 {
        flags |= DH_MODULUS_TOO_LARGE;
    }
    if !is_prime(p, 0) {
        flags |= DH_CHECK_P_NOT_PRIME;
    } else if !is_prime(&(p >> 1usize), 0) {
        flags |= DH_CHECK_P_NOT_SAFE_PRIME;
    }
    flags
}

/// `DH_generate_parameters_ex(bits, generator)`: a safe prime with OpenSSL's congruence.
pub(crate) fn dh_generate(bits: usize, generator: u32) -> Result<BigUint, CipherError> {
    let (add, rem) = match generator {
        2 => (24u32, 23u32),
        5 => (60, 59),
        _ => (12, 11),
    };
    generate(bits, true, Some(&BigUint::from(add)), Some(&BigUint::from(rem)))
}

/// `ops.primeGenerate(bits, safe, add|null, rem|null, async) -> bytes | Promise<bytes>`
fn op_prime_generate(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let bits = arg_f64(args, 0, ctx)? as usize;
    let safe = args.get_or_undefined(1).to_boolean();
    let opt = |v: &JsValue, ctx: &mut Context| -> JsResult<Option<BigUint>> {
        if v.is_null_or_undefined() { Ok(None) } else { Ok(Some(BigUint::from_bytes_be(&bytes_arg(v, ctx)?))) }
    };
    let add = opt(args.get_or_undefined(2), ctx)?;
    let rem = opt(args.get_or_undefined(3), ctx)?;
    let asynchronous = args.get_or_undefined(4).to_boolean();
    job(ctx, asynchronous, move || generate(bits, safe, add.as_ref(), rem.as_ref()).map(|p| p.to_bytes_be()))
}

/// `ops.primeCheck(bytes, checks, async) -> bytes ([1] or [0]) | Promise`
fn op_prime_check(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let n = BigUint::from_bytes_be(&bytes_arg(args.get_or_undefined(0), ctx)?);
    let checks = arg_f64(args, 1, ctx)? as usize;
    let asynchronous = args.get_or_undefined(2).to_boolean();
    job(ctx, asynchronous, move || Ok(vec![u8::from(is_prime(&n, checks))]))
}

/// `ops.dhCheck(prime, generator) -> verifyError flags`
fn op_dh_check(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let p = BigUint::from_bytes_be(&bytes_arg(args.get_or_undefined(0), ctx)?);
    let g = BigUint::from_bytes_be(&bytes_arg(args.get_or_undefined(1), ctx)?);
    Ok(JsValue::from(dh_check(&p, &g)))
}

/// `ops.dhGenerate(bits, generator) -> prime bytes`
fn op_dh_generate(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let bits = arg_f64(args, 0, ctx)? as usize;
    let g = arg_f64(args, 1, ctx)?.to_u32().unwrap_or(2);
    job(ctx, false, move || dh_generate(bits, g).map(|p| p.to_bytes_be()))
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("primeGenerate", 5, op_prime_generate),
    ("primeCheck", 3, op_prime_check),
    ("dhCheck", 2, op_dh_check),
    ("dhGenerate", 2, op_dh_generate),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primes_follow_openssl_rules() {
        assert!(is_prime(&BigUint::from(7u32), 0));
        assert!(!is_prime(&BigUint::from(9u32), 0));
        assert!(is_prime(&((BigUint::one() << 127usize) - BigUint::one()), 0));
        assert!(!is_prime(&((BigUint::one() << 128usize) + BigUint::one()), 10));
        let p = generate(64, false, None, None).unwrap();
        assert_eq!(p.bits(), 64);
        assert!(((&p >> 62usize) & BigUint::one()).is_one());
        let s = generate(48, true, None, None).unwrap();
        assert!(is_prime(&(&s >> 1usize), 0));
        let a = generate(32, false, Some(&BigUint::from(12u32)), Some(&BigUint::from(11u32))).unwrap();
        assert_eq!((a % 12u32).to_u32(), Some(11));
        assert_eq!(generate(1, false, None, None).unwrap_err().message, "error:01800076:bignum routines::bits too small");
        let g = dh_generate(64, 2).unwrap();
        assert_eq!((&g % 24u32).to_u32(), Some(23));
        assert_eq!(dh_check(&BigUint::from(23u32), &BigUint::from(2u32)), 128);
        assert_eq!(dh_check(&BigUint::from(21u32), &BigUint::from(2u32)), 129);
        assert_eq!(dh_check(&BigUint::from(13u32), &BigUint::from(2u32)), 130);
        assert_eq!(dh_check(&BigUint::from(1019u32), &BigUint::from(1019u32)), 136);
    }
}
