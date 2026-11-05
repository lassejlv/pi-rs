//! `crypto.generatePrime`/`checkPrime` (OpenSSL's add/rem/safe rules and errors) and
//! `createDiffieHellman` with generated primes and Node's `verifyError` (OpenSSL 3 `DH_check`).
//! Expected output is Node 22's.

use std::process::Command;

const NODE_BIN: &str = env!("CARGO_BIN_EXE_pi-jsrt-node");

#[test]
fn primes_and_dh_match_node() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prime.js");
    std::fs::write(&path, SCRIPT).unwrap();
    let out = Command::new(NODE_BIN).arg(&path).output().unwrap();
    assert_eq!(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)), EXPECTED);
}

const SCRIPT: &str = r###"const c = require("crypto");
const t = (label, f) => { try { const r = f(); console.log(label, typeof r === "bigint" ? r + "n" : r instanceof ArrayBuffer ? "AB " + Buffer.from(r).toString("hex") : Buffer.isBuffer(r) ? r.toString("hex") : r); } catch (e) { console.log(label, "ERR", e.name, e.code, e.message); } };
t("check 7", () => c.checkPrimeSync(7n));
t("check 9", () => c.checkPrimeSync(9n));
t("check 1", () => c.checkPrimeSync(1n));
t("check 0", () => c.checkPrimeSync(0n));
t("check 2", () => c.checkPrimeSync(2n));
t("check buf", () => c.checkPrimeSync(Buffer.from([0x0d])));
t("check big", () => c.checkPrimeSync(2n ** 127n - 1n));
t("check bigc", () => c.checkPrimeSync(2n ** 128n + 1n, { checks: 10 }));
t("check neg", () => c.checkPrimeSync(-7n));
t("check str", () => c.checkPrimeSync("7"));
t("check checks", () => c.checkPrimeSync(7n, { checks: -1 }));
t("check opts", () => c.checkPrimeSync(7n, "x"));
t("gen 0", () => c.generatePrimeSync(0));
t("gen 1", () => c.generatePrimeSync(1));
t("gen 2", () => c.generatePrimeSync(2, { bigint: true }));
t("gen 3", () => c.generatePrimeSync(3, { bigint: true }) > 0n);
t("gen str", () => c.generatePrimeSync("8"));
t("gen bits", () => { const p = c.generatePrimeSync(64, { bigint: true }); return p.toString(2).length + " " + c.checkPrimeSync(p); });
t("gen ab", () => { const p = c.generatePrimeSync(32); return (p instanceof ArrayBuffer) + " " + p.byteLength; });
t("gen safe", () => { const p = c.generatePrimeSync(32, { safe: true, bigint: true }); return p.toString(2).length + " " + c.checkPrimeSync((p - 1n) / 2n) + " " + (p % 4n); });
t("gen add", () => { const p = c.generatePrimeSync(32, { add: 12n, rem: 11n, bigint: true }); return p % 12n; });
t("gen add only", () => { const p = c.generatePrimeSync(32, { add: 12n, bigint: true }); return p % 12n; });
t("gen rem>add", () => c.generatePrimeSync(32, { add: 12n, rem: 13n }));
t("gen rem only", () => c.generatePrimeSync(32, { rem: 3n }).byteLength);
t("gen add big", () => c.generatePrimeSync(8, { add: 1n << 9n }));
t("gen add buf", () => { const p = c.generatePrimeSync(32, { add: Buffer.from([12]), rem: Buffer.from([7]), bigint: true }); return p % 12n; });
t("dh 23", () => { const d = c.createDiffieHellman(Buffer.from([23]), 2); return d.verifyError; });
t("dh 23 g5", () => c.createDiffieHellman(Buffer.from([23]), 5).verifyError);
t("dh 21", () => c.createDiffieHellman(Buffer.from([21]), 2).verifyError);
t("dh 13", () => c.createDiffieHellman(Buffer.from([13]), 2).verifyError);
t("dh 7 g3", () => c.createDiffieHellman(Buffer.from([7]), 3).verifyError);
t("dh 1019 g2", () => c.createDiffieHellman(Buffer.from([0x03, 0xfb]), 2).verifyError);
t("dh 1019 g3", () => c.createDiffieHellman(Buffer.from([0x03, 0xfb]), 3).verifyError);
t("dh 1019 g1", () => c.createDiffieHellman(Buffer.from([0x03, 0xfb]), 1).verifyError);
t("dh 1019 g1019", () => c.createDiffieHellman(Buffer.from([0x03, 0xfb]), Buffer.from([0x03, 0xfb])).verifyError);
t("dh gen 256", () => { const d = c.createDiffieHellman(256); return d.getPrime().length + " " + d.verifyError + " " + d.getGenerator("hex"); });
t("dh gen 512", () => { const d = c.createDiffieHellman(512); const p = BigInt("0x" + d.getPrime("hex")); return d.getPrime().length + " " + d.verifyError + " " + (p % 24n) + " " + d.getGenerator("hex"); });
t("dh gen 512 g1", () => c.createDiffieHellman(512, 1));
t("dh gen 0", () => c.createDiffieHellman(0));
t("dh gen 2", () => c.createDiffieHellman(2));
t("dh gen 1.5", () => c.createDiffieHellman(1.5));
t("dh group", () => c.getDiffieHellman("modp14").verifyError);
t("dh keys", () => { const a = c.createDiffieHellman(Buffer.from([0x03, 0xfb]), 2); const ak = a.generateKeys(); return ak.length; });
c.generatePrime(16, { bigint: true }, (err, p) => {
  console.log("async gen", err, typeof p, c.checkPrimeSync(p));
  c.checkPrime(17n, (err, r) => console.log("async check", err, r));
});
t("async check nocb", () => c.checkPrime(17n));
"###;

const EXPECTED: &str = r###"check 7 true
check 9 false
check 1 false
check 0 false
check 2 true
check buf true
check big true
check bigc false
check neg ERR RangeError ERR_OUT_OF_RANGE The value of "candidate" is out of range. It must be >= 0. Received -7n
check str ERR TypeError ERR_INVALID_ARG_TYPE The "candidate" argument must be of type bigint or an instance of ArrayBuffer, TypedArray, Buffer, or DataView. Received type string ('7')
check checks ERR RangeError ERR_OUT_OF_RANGE The value of "options.checks" is out of range. It must be >= 0 && <= 2147483647. Received -1
check opts ERR TypeError ERR_INVALID_ARG_TYPE The "options" argument must be of type object. Received type string ('x')
gen 0 ERR RangeError ERR_OUT_OF_RANGE The value of "size" is out of range. It must be >= 1 && <= 2147483647. Received 0
gen 1 ERR Error undefined error:01800076:bignum routines::bits too small
gen 2 3n
gen 3 true
gen str ERR TypeError ERR_INVALID_ARG_TYPE The "size" argument must be of type number. Received type string ('8')
gen bits 64 true
gen ab true 4
gen safe 32 true 3
gen add 11n
gen add only 1n
gen rem>add ERR RangeError ERR_OUT_OF_RANGE invalid options.rem
gen rem only 4
gen add big ERR RangeError ERR_OUT_OF_RANGE invalid options.add
gen add buf 7n
dh 23 128
dh 23 g5 128
dh 21 129
dh 13 130
dh 7 g3 128
dh 1019 g2 128
dh 1019 g3 128
dh 1019 g1 ERR Error ERR_OSSL_DH_BAD_GENERATOR error:02800065:Diffie-Hellman routines::bad generator
dh 1019 g1019 136
dh gen 256 ERR TypeError ERR_INVALID_ARG_VALUE Invalid DH parameters
dh gen 512 64 0 23 02
dh gen 512 g1 ERR Error ERR_OSSL_DH_BAD_GENERATOR error:02800065:Diffie-Hellman routines::bad generator
dh gen 0 ERR Error ERR_OSSL_DH_MODULUS_TOO_SMALL error:0280007E:Diffie-Hellman routines::modulus too small
dh gen 2 ERR TypeError ERR_INVALID_ARG_VALUE Invalid DH parameters
dh gen 1.5 ERR RangeError ERR_OUT_OF_RANGE The value of "sizeOrKey" is out of range. It must be an integer. Received 1.5
dh group 0
dh keys ERR Error ERR_CRYPTO_OPERATION_FAILED Key generation failed
async check nocb ERR TypeError ERR_INVALID_ARG_TYPE The "callback" argument must be of type function. Received undefined
async gen undefined bigint true
async check undefined true
"###;
