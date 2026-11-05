//! `node:crypto` algorithm coverage against Node 22 (OpenSSL 3 default provider): every cipher
//! from `getCiphers()` (AES/ARIA/Camellia/SM4/3DES in ECB/CBC/CFB/CFB8/CFB1/OFB/CTR/GCM/CCM/OCB/
//! XTS, key wraps, ChaCha20(-Poly1305)) with `getCipherInfo`, mode-specific errors, and the
//! digests (SHA-3, SHAKE with outputLength, BLAKE2, RIPEMD-160, SM3, MD5-SHA1) in hashes, HMAC,
//! PBKDF2 and HKDF. Expected output is Node's.

use std::process::Command;

const NODE_BIN: &str = env!("CARGO_BIN_EXE_pi-jsrt-node");

fn run(script: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.js");
    std::fs::write(&path, script).unwrap();
    let out = Command::new(NODE_BIN).arg(&path).output().unwrap();
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

#[test]
fn ciphers_match_node() {
    assert_eq!(run(CIPHERS), CIPHERS_EXPECTED);
}

#[test]
fn cipher_modes_match_node() {
    assert_eq!(run(MODES), MODES_EXPECTED);
}

#[test]
fn hashes_match_node() {
    assert_eq!(run(HASHES), HASHES_EXPECTED);
}

const CIPHERS: &str = r###"const crypto = require("crypto");
const pt = Buffer.from("The quick brown fox jumps over the lazy dog. 0123456789abcdef!");
const out = [];
for (const name of crypto.getCiphers()) {
  const info = crypto.getCipherInfo(name);
  const row = [name, JSON.stringify(info)];
  try {
    const key = Buffer.alloc(info.keyLength).map((_, i) => i * 7 + 1);
    const iv = info.ivLength ? Buffer.alloc(info.ivLength).map((_, i) => 0xf0 - i) : null;
    const aead = /gcm|ccm|ocb|poly1305/i.test(info.mode) || /poly1305/.test(name);
    const opts = aead ? { authTagLength: 16 } : undefined;
    const data = /wrap/.test(name) ? pt.subarray(0, /pad/.test(name) ? 21 : 24) : /xts/.test(name) ? pt.subarray(0, 40) : pt;
    const c = crypto.createCipheriv(name, key, iv, opts);
    if (aead) c.setAAD(Buffer.from("aad"), { plaintextLength: data.length });
    let ct;
    if (/ccm|wrap|xts/.test(info.mode) || /wrap|xts/.test(name)) ct = Buffer.concat([c.update(data), c.final()]);
    else ct = Buffer.concat([c.update(data.subarray(0, 5)), c.update(data.subarray(5, 37)), c.update(data.subarray(37)), c.final()]);
    row.push(/cms3des/i.test(info.name) ? ct.length : ct.toString("hex"));
    let tag;
    if (aead) row.push((tag = c.getAuthTag()).toString("hex"));
    const d = crypto.createDecipheriv(name, key, iv, opts);
    if (aead) { d.setAuthTag(tag); d.setAAD(Buffer.from("aad"), { plaintextLength: data.length }); }
    const back = Buffer.concat([d.update(ct), d.final()]);
    row.push(back.equals(data));
  } catch (e) {
    row.push("ERR " + e.code + " " + e.message);
  }
  out.push(row.join(" | "));
}
console.log(out.join("\n"));
"###;

const CIPHERS_EXPECTED: &str = r###"aes-128-cbc | {"mode":"cbc","name":"aes-128-cbc","nid":419,"blockSize":16,"ivLength":16,"keyLength":16} | 044652c954d259d3fc2f379d8587f0352246dfd3c6437a41a98c4193b9c83aae3736214bfc9281b03f3b46b7b96f6e06f5a9af3ad275e16e161189654c4d864d | true
aes-128-cbc-hmac-sha1 | {"mode":"cbc","name":"aes-128-cbc-hmac-sha1","nid":916,"blockSize":16,"ivLength":16,"keyLength":16} | ERR undefined Trying to add data in unsupported state
aes-128-cbc-hmac-sha256 | {"mode":"cbc","name":"aes-128-cbc-hmac-sha256","nid":948,"blockSize":16,"ivLength":16,"keyLength":16} | ERR undefined Trying to add data in unsupported state
aes-128-ccm | {"mode":"ccm","name":"id-aes128-ccm","nid":896,"blockSize":1,"ivLength":12,"keyLength":16} | 11524cc9737ef6ef599ed129a3490c89e3fe7f71b0e60f66dc902b11dce69a291ffdf45976b47ecf97aea88f25bbfbe1837d49a6d45484b7ced6448c4717 | 50109353b4ba1db1039eae817dabcc81 | true
aes-128-cfb | {"mode":"cfb","name":"aes-128-cfb","nid":421,"blockSize":1,"ivLength":16,"keyLength":16} | 3315e3dfe6c351701de45724174e86391d5c6dab7099d2b4c6e6c3e7bc373d66867b7716a7506585ee859d4c3fe51825cb5bd07b1ea62d7fc770fe62a236 | true
aes-128-cfb1 | {"mode":"cfb","name":"aes-128-cfb1","nid":650,"blockSize":1,"ivLength":16,"keyLength":16} | 530c4cedbeb2a6de09fbb6a9e93ff365681b5fdaa98c06ae0963dff0072026c7645e1cc00aafadecf4ee9c1895e57f557389f617dbc92eb26316fd698d13 | true
aes-128-cfb8 | {"mode":"cfb","name":"aes-128-cfb8","nid":653,"blockSize":1,"ivLength":16,"keyLength":16} | 33289760727529ba54c1f22b7709587d83f3c8f10808bbcf20c8dfb59de129ec82173681bd7a7a35f5e3e0a4f93538dd739787c0315b2b51378b3c883017 | true
aes-128-ctr | {"mode":"ctr","name":"aes-128-ctr","nid":904,"blockSize":1,"ivLength":16,"keyLength":16} | 3315e3dfe6c351701de45724174e8639966188bedb607007e4efab4d90c4942905b92da6dc0b24d48e5b0d08b44b17f90981c7bc887a24287f9b6ddb6a0b | true
aes-128-ecb | {"mode":"ecb","name":"aes-128-ecb","nid":418,"blockSize":16,"keyLength":16} | 6f2801dd454b6a52377651b9ad05087ecf34d37eafe05cdc17231ef63c66543d101174f9d38216fa01e0e67bef13195fcae4f8e14ceb12ab841c3653976722aa | true
aes-128-gcm | {"mode":"gcm","name":"id-aes128-gcm","nid":895,"blockSize":1,"ivLength":12,"keyLength":16} | 0cb9138bc8c288bfed96195d4453d49f6e95356b6a04c74cbda7a93ee12a480dac24b31ed9a4d2908f9dfdb77dba3a56cd4fe661c0fcb608cc67dd35b2ec | aad803e182b7709e34630adfb1fe0062 | true
aes-128-ocb | {"mode":"ocb","name":"aes-128-ocb","nid":958,"blockSize":16,"ivLength":12,"keyLength":16} | e4f4bb3b8655827c9a1ed26b8ea4516b308bc63ceaeb097f69f52f60b51cd0104df213497116575dcb55c95d6ab9c25bbb295766ddddd6f5472ef8e3b684 | 3806a637e587e19ac9054562f6d13f0e | true
aes-128-ofb | {"mode":"ofb","name":"aes-128-ofb","nid":420,"blockSize":1,"ivLength":16,"keyLength":16} | 3315e3dfe6c351701de45724174e8639830798e3315fe844d8fa4d4896fbefd70f632c11d7d89441d94f161d5bf2f7d452a5bc403e0459e87c4cc9a073fe | true
aes-128-xts | {"mode":"xts","name":"aes-128-xts","nid":913,"blockSize":1,"ivLength":16,"keyLength":32} | 1267a8fc76e33ab0d8ffa52e5a683ccf96ad659d19023f6ab1f7938c7d9e01e34b83d5cf041b9298 | true
aes-192-cbc | {"mode":"cbc","name":"aes-192-cbc","nid":423,"blockSize":16,"ivLength":16,"keyLength":24} | a4b99a9704d2f883c4396ef267dc380db43fc38793490986478f7737a5a044f3a4bc4b84b6bc17c38f089746f71148b65b844baf9a8d408261d3a4f46e527083 | true
aes-192-ccm | {"mode":"ccm","name":"id-aes192-ccm","nid":899,"blockSize":1,"ivLength":12,"keyLength":24} | 140839928da4fc312e7c3cc9065cf189f5da76ff38724277f041e3bccb0a1bb86d43d1b8136a3df2cfab4c6d97b5e43fae99e951a0a2d859e172c74f1830 | 12646026ae3fa0a4f504446171627e18 | true
aes-192-cfb | {"mode":"cfb","name":"aes-192-cfb","nid":425,"blockSize":1,"ivLength":16,"keyLength":24} | 9b1663c5906a8e2c1751ed1c438cfa057620782e7bbcdd3d4fac12286adceee3c80f15d745edf5cf45222c2dfb01ed6772589c7c4f207ce249ebb07c5e9f | true
aes-192-cfb1 | {"mode":"cfb","name":"aes-192-cfb1","nid":651,"blockSize":1,"ivLength":16,"keyLength":24} | d6fe85dce6ebb7af71c8050fdb76e79a142be8e56519d0035970ddc076ccf39067d41faceb600c0f82f941b9d78a4e2978e196ddd4bcc14312eea3212903 | true
aes-192-cfb8 | {"mode":"cfb","name":"aes-192-cfb8","nid":654,"blockSize":1,"ivLength":16,"keyLength":24} | 9bbf900f6b7ef371940f605ddc3134e0961b6195357ca00345d355e1b1a250f1efb2220cc874a0f65345647ef39388f060281d8a1324b32ca62685f752de | true
aes-192-ctr | {"mode":"ctr","name":"aes-192-ctr","nid":905,"blockSize":1,"ivLength":16,"keyLength":24} | 9b1663c5906a8e2c1751ed1c438cfa0511060f5853f53be732ce7dc234a3914c4e4737c005c40dc3585fd6838ef79e807d47238bd6caa8e5cdd185fd5343 | true
aes-192-ecb | {"mode":"ecb","name":"aes-192-ecb","nid":422,"blockSize":16,"keyLength":24} | 97a63bfe6c8d5a087b2a314d7aa614de3d71091eb333697222a71c626463d077bab4ba1d01fdf240ebed21352658a21295c4e1b5b0a72fcad57ba2e753c3882e | true
aes-192-gcm | {"mode":"gcm","name":"id-aes192-gcm","nid":898,"blockSize":1,"ivLength":12,"keyLength":24} | 0f1818b077d843effb2a97cdf787d4cda8382c9695a5fe3bd2ea40a3abcaacc529400fc15cb6562f7a7fdb76da556a248586c0495b8fb21997d7b23cbe82 | 3fcea7282fba8f9e3aa2d30fbdb7dfeb | true
aes-192-ocb | {"mode":"ocb","name":"aes-192-ocb","nid":959,"blockSize":16,"ivLength":12,"keyLength":24} | 1cf01ea50f6e7eb6cd315ae7f61f034eb7c3ab4a217b5144b1dd429cf4a5e17e6b29a645ef361d5d370a0452a0b94b1a807be5829d205d106df20b546f5e | 6587c9cf831de05cecd3bfa483ac1417 | true
aes-192-ofb | {"mode":"ofb","name":"aes-192-ofb","nid":424,"blockSize":1,"ivLength":16,"keyLength":24} | 9b1663c5906a8e2c1751ed1c438cfa05c996d03f151690d8688e05a0b8810d54e6e28bd5488316de01bd068f712b454b97301154f70d93a7529a851530b6 | true
aes-256-cbc | {"mode":"cbc","name":"aes-256-cbc","nid":427,"blockSize":16,"ivLength":16,"keyLength":32} | 492b558c865033950fecde89dc4c79c1ac8fa34fd96d2e8972c2fcbc444a509621f6a118b76f23ee713e79891a46793a4ff62302c00972d874f52141284e30bf | true
aes-256-cbc-hmac-sha1 | {"mode":"cbc","name":"aes-256-cbc-hmac-sha1","nid":918,"blockSize":16,"ivLength":16,"keyLength":32} | ERR undefined Trying to add data in unsupported state
aes-256-cbc-hmac-sha256 | {"mode":"cbc","name":"aes-256-cbc-hmac-sha256","nid":950,"blockSize":16,"ivLength":16,"keyLength":32} | ERR undefined Trying to add data in unsupported state
aes-256-ccm | {"mode":"ccm","name":"id-aes256-ccm","nid":902,"blockSize":1,"ivLength":12,"keyLength":32} | 0af54062d614e6bd5f76189981930313bc572d3d653a5c663030c5811995765107e88f8a69052e53c81b44ba88620b05d34ee18b4d06cc004be8cd590a05 | 3e245de87a90f3b74c5a3b9d6a99706b | true
aes-256-cfb | {"mode":"cfb","name":"aes-256-cfb","nid":429,"blockSize":1,"ivLength":16,"keyLength":32} | 9187d89b14906b7ea22f57faa7f1f6a4335a7dcd5d2a822197d0b6150d6c750bc569759c2f4380d285e2c96a4d6f584986a85aa16319c13316a6b7a22c34 | true
aes-256-cfb1 | {"mode":"cfb","name":"aes-256-cfb1","nid":652,"blockSize":1,"ivLength":16,"keyLength":32} | b045b2c490a15be2af505c914a7a30abc279034b641367404138c044c4c42abdda723493996fc16bc3b13cf319c761066aca903fea04fa38bbdc2e605272 | true
aes-256-cfb8 | {"mode":"cfb","name":"aes-256-cfb8","nid":655,"blockSize":1,"ivLength":16,"keyLength":32} | 91ec6f13d234fec21986927bbbf9cad4ec0cf17b1da588dc02948e5f452d2475627463e804c73e686f0ab6da85bcb336f1f386d43dd51baa67dcb860d749 | true
aes-256-ctr | {"mode":"ctr","name":"aes-256-ctr","nid":906,"blockSize":1,"ivLength":16,"keyLength":32} | 9187d89b14906b7ea22f57faa7f1f6a406ad65bfd8a1e5151d40743d4b63b9dc139e7af9d52fc28acdd973b795cc494bbfdbbe1e5c45b84c3d266529aa2d | true
aes-256-ecb | {"mode":"ecb","name":"aes-256-ecb","nid":426,"blockSize":16,"keyLength":32} | c463a07d147a7f981aa3385ebab470a0cb2d66fb7af0b9bdbc069f4a1a647140f1f5489e0775e2ffe49fedf5054d4cf10017e8e125ac44aee534933206f44e0a | true
aes-256-gcm | {"mode":"gcm","name":"id-aes256-gcm","nid":901,"blockSize":1,"ivLength":12,"keyLength":32} | 68c51b8f9c603f29b8c7d3dde2d5461f74f1cfdae1487a0a532520c55ba20fb14f3b7573f17fe1d143e5dcf49808e850b32f07bb57cdf2cf0526810659c3 | f073671c740a17f5b371328a5a1dfcca | true
aes-256-ocb | {"mode":"ocb","name":"aes-256-ocb","nid":960,"blockSize":16,"ivLength":12,"keyLength":32} | 1982d72ae611e5531c0f71afcd38d9085c3b5c26bbbf378d5d6a88326d1dffe1b1e872ca2df6f29f68d7f3f0968b0c9d48611a22edaf9a27ae8049549788 | 5b86124188180f1904c8efac5d293759 | true
aes-256-ofb | {"mode":"ofb","name":"aes-256-ofb","nid":428,"blockSize":1,"ivLength":16,"keyLength":32} | 9187d89b14906b7ea22f57faa7f1f6a4093fd7ae5690ec67a4921dcebe1226782d0fc0a225902f36767d8760a43b967e7fb937133bf93dd5b22183a151a4 | true
aes-256-xts | {"mode":"xts","name":"aes-256-xts","nid":914,"blockSize":1,"ivLength":16,"keyLength":64} | a6628a6abbb6d54a42cff6001d62d9ae8d9ce62be1d3165df1c7c8d2bb79dc6a412692389e44141a | true
aes128 | {"mode":"cbc","name":"aes-128-cbc","nid":419,"blockSize":16,"ivLength":16,"keyLength":16} | 044652c954d259d3fc2f379d8587f0352246dfd3c6437a41a98c4193b9c83aae3736214bfc9281b03f3b46b7b96f6e06f5a9af3ad275e16e161189654c4d864d | true
aes128-wrap | {"mode":"wrap","name":"id-aes128-wrap","nid":788,"blockSize":8,"ivLength":8,"keyLength":16} | 32a77077f4766052d8f23cfaf82aee4f46fcdb7fe47c22de151a895b22f1c520 | true
aes128-wrap-pad | {"mode":"wrap","name":"id-aes128-wrap-pad","nid":897,"blockSize":8,"ivLength":4,"keyLength":16} | 84e25828f31d586af0afd8822c91cf0ec3b74a46aaad88ea117a8db423c02244 | true
aes192 | {"mode":"cbc","name":"aes-192-cbc","nid":423,"blockSize":16,"ivLength":16,"keyLength":24} | a4b99a9704d2f883c4396ef267dc380db43fc38793490986478f7737a5a044f3a4bc4b84b6bc17c38f089746f71148b65b844baf9a8d408261d3a4f46e527083 | true
aes192-wrap | {"mode":"wrap","name":"id-aes192-wrap","nid":789,"blockSize":8,"ivLength":8,"keyLength":24} | 9cc8b6b457048148af60d37a7a8edd8131e5d3f8696c57ca65da324236e3c90a | true
aes192-wrap-pad | {"mode":"wrap","name":"id-aes192-wrap-pad","nid":900,"blockSize":8,"ivLength":4,"keyLength":24} | 69794ac36318df8692c68d46e366ec89f14c2b7ccd7185b1158bcb74470457f6 | true
aes256 | {"mode":"cbc","name":"aes-256-cbc","nid":427,"blockSize":16,"ivLength":16,"keyLength":32} | 492b558c865033950fecde89dc4c79c1ac8fa34fd96d2e8972c2fcbc444a509621f6a118b76f23ee713e79891a46793a4ff62302c00972d874f52141284e30bf | true
aes256-wrap | {"mode":"wrap","name":"id-aes256-wrap","nid":790,"blockSize":8,"ivLength":8,"keyLength":32} | ba504a5d4f052cc0da8f506d881375c2a1b085ff10e82b9c25f5d76c350223e8 | true
aes256-wrap-pad | {"mode":"wrap","name":"id-aes256-wrap-pad","nid":903,"blockSize":8,"ivLength":4,"keyLength":32} | bc61761a1d71930c19af652b312769fb7e2bbf3b82e039a9a372926bdae2b62b | true
aria-128-cbc | {"mode":"cbc","name":"aria-128-cbc","nid":1066,"blockSize":16,"ivLength":16,"keyLength":16} | 7e1d84f79eae835ee9c0f2f8bebfa027a715542ba533a4a298d1bec5168729d67c71f92043a12912099ceb8a9de1dc29051ba31a0291077875b90d524ad4c6e3 | true
aria-128-ccm | {"mode":"ccm","name":"aria-128-ccm","nid":1120,"blockSize":1,"ivLength":12,"keyLength":16} | b977996c9212424cd7a9b118028817098e190fa0f7fbbce0fd095f024ca4e0306d506c50d23f45614b6178502c78127d04e91bec47d9152bbc39da82655a | ebb4acc0af3f636ae0a7efdf623c6033 | true
aria-128-cfb | {"mode":"cfb","name":"aria-128-cfb","nid":1067,"blockSize":1,"ivLength":16,"keyLength":16} | ba5525818e053bbee170ee4dfb37281680abd9882cf35593c6452a42ed8ccbb384ee72cc7c4bf353187b021f6878236b964ecac1f9455905dad93171e096 | true
aria-128-cfb1 | {"mode":"cfb","name":"aria-128-cfb1","nid":1080,"blockSize":1,"ivLength":16,"keyLength":16} | c98267a07c7e02bfb11cf36299f4be23f22895b85d5d818100b923557b513ceeec1711356e7bb8c2bb2e6d8336ab0184bc1cb33d34b5fd3711d1164f8cf0 | true
aria-128-cfb8 | {"mode":"cfb","name":"aria-128-cfb8","nid":1083,"blockSize":1,"ivLength":16,"keyLength":16} | ba449a37464dfd49bc5a15110c3b5ea98ca0ed11c132b96947b647e1f5f3ec65149cc7e71c7a99d949c602aa8e954717b67149dbdd420398a295c65f39f6 | true
aria-128-ctr | {"mode":"ctr","name":"aria-128-ctr","nid":1069,"blockSize":1,"ivLength":16,"keyLength":16} | ba5525818e053bbee170ee4dfb3728162487cbfd84285b41b63ff1fe217d15312f14adf19dfccd78b38f90bfef737da468f36c8d95ea73b4e10d92b92887 | true
aria-128-ecb | {"mode":"ecb","name":"aria-128-ecb","nid":1065,"blockSize":16,"keyLength":16} | ba8dce56434262b13776f0c9bbc32bd5831a45bc1117262e7cd524977796e8835aade84e5c573ba761c3e8fe7925ad5c3a2f64a7d5c071ce97a0de236385f08e | true
aria-128-gcm | {"mode":"gcm","name":"aria-128-gcm","nid":1123,"blockSize":1,"ivLength":12,"keyLength":16} | a0b9cb5f9ced07946a0b0f16d4fecefc1c899a61145491933826b07440aefa271397494ccec1a30c36e58cc41776ee196565adbefc91fe12daebd50207ad | f399b11c44d840cb2d5111cedbdab846 | true
aria-128-ofb | {"mode":"ofb","name":"aria-128-ofb","nid":1068,"blockSize":1,"ivLength":16,"keyLength":16} | ba5525818e053bbee170ee4dfb372816334d1d17619d636c06bf1febc652637e1447dffd1015ce53ce2aed30a05547f07779431ae83c0505ee55a1335991 | true
aria-192-cbc | {"mode":"cbc","name":"aria-192-cbc","nid":1071,"blockSize":16,"ivLength":16,"keyLength":24} | 814d1bca3abe0ace5abd660c102d17bb3196152e3f43a0ac4cec5afdf10f4fda8a385262cded390d015a2f03165f51aa19064e77b5249d94ed805a2d67455688 | true
aria-192-ccm | {"mode":"ccm","name":"aria-192-ccm","nid":1121,"blockSize":1,"ivLength":12,"keyLength":24} | cd0076678ca541519b2d9293b0c022958a0d3b700daa494be4a19be6a3c89e32a52247ab7476c53f5787dc3261c44702f979411c783cae6c8b70644fddc5 | 8058a3ba4f1b46e394738698fdb916d2 | true
aria-192-cfb | {"mode":"cfb","name":"aria-192-cfb","nid":1072,"blockSize":1,"ivLength":16,"keyLength":24} | 78040710fe93130ca10c67b53c79afee3b50b09f78e5812c4d91b00b40cf7387034a98f26f43c1d0b3f12822ca0b07992521e3918fd39cdc57653b9885f5 | true
aria-192-cfb1 | {"mode":"cfb","name":"aria-192-cfb1","nid":1081,"blockSize":1,"ivLength":16,"keyLength":24} | 7f62e1d390bf6fa0813bf7d5ee8d4615bf7c614caed71ece03534307ee43aebad1fb146d8ee31a7069b7da343ac9a582636df3225d0078f9b1d63d0af5e4 | true
aria-192-cfb8 | {"mode":"cfb","name":"aria-192-cfb8","nid":1084,"blockSize":1,"ivLength":16,"keyLength":24} | 78cf428256dd6b58593dab4753db6a1666aa35b7eae74c12be72dda02c40369664805ada4d0a4c39716c138055a1b2d11c41f498ea1247059fb6d4011497 | true
aria-192-ctr | {"mode":"ctr","name":"aria-192-ctr","nid":1074,"blockSize":1,"ivLength":16,"keyLength":24} | 78040710fe93130ca10c67b53c79afee828b89b5ee72287b36626906e9de82c9e92d8106c0f1e1050f98c7da116ee58097d03f2397949c82d1fb4329637a | true
aria-192-ecb | {"mode":"ecb","name":"aria-192-ecb","nid":1070,"blockSize":16,"keyLength":24} | 0915ae1a83191fb651264d2dd805c6315d4161e2c7736e1beb2f909111010ad2904878d8c5d313bd0de935ddf100b6525f81629efc5abe8e77727b20a7947bbe | true
aria-192-gcm | {"mode":"gcm","name":"aria-192-gcm","nid":1124,"blockSize":1,"ivLength":12,"keyLength":24} | 76fdf194314609ee6defee1a644ffd68cd1d984fcd72d82ebdf87412b450562708b2af175da66e116c464c54be4b524b717ae54880e22da6ccc54bfab5ff | 1aa0882ee2ffdea59b2d47ab34923003 | true
aria-192-ofb | {"mode":"ofb","name":"aria-192-ofb","nid":1073,"blockSize":1,"ivLength":16,"keyLength":24} | 78040710fe93130ca10c67b53c79afeee66b93c6ac697c598112dd0f54e608e681567ff79a4712f766d3afe9501010fff46b0b910b168ae5cb24df1d2d86 | true
aria-256-cbc | {"mode":"cbc","name":"aria-256-cbc","nid":1076,"blockSize":16,"ivLength":16,"keyLength":32} | da328e56b50ea49761de9d0d0d6b954788fde60c5b30f1febe149e71bf769832b332d95ed43aacd06aba67d24d9875b8121a8a6d043b9375cd4dce9301fc229b | true
aria-256-ccm | {"mode":"ccm","name":"aria-256-ccm","nid":1122,"blockSize":1,"ivLength":12,"keyLength":32} | f432b0be683c75090c42c8024b5f5a50ee5c876b42bc26af7c0fd05832070fe26a9474d2eeb0a7be194522bb65c2251c5c4019d688eeb81092e131f70045 | f006d17c3d052850bce7196a598c5d06 | true
aria-256-cfb | {"mode":"cfb","name":"aria-256-cfb","nid":1077,"blockSize":1,"ivLength":16,"keyLength":32} | 0dc26e7a2d370d024f8e6f5534afcd5f8c645cd83d29dc674259b940f9851f05230e3d02617bbd509286aeb5d5e4b0733211327b9c9f79364f035f604478 | true
aria-256-cfb1 | {"mode":"cfb","name":"aria-256-cfb1","nid":1082,"blockSize":1,"ivLength":16,"keyLength":32} | 4339824879043c63799d13bdf5cdd00e91fbbe482b4233832ace747c80f5347801c851350ae072e938ea2774380c2dc2ae1092930e5980f44d06d19f120a | true
aria-256-cfb8 | {"mode":"cfb","name":"aria-256-cfb8","nid":1085,"blockSize":1,"ivLength":16,"keyLength":32} | 0d486d1ce0c8fa60772d5d900218f1c838640cc7ec31c9cbf375057a6b01035c93f125dac904bc3593938c328aa3cb3398dfbac1907c72d20a63d827cbbf | true
aria-256-ctr | {"mode":"ctr","name":"aria-256-ctr","nid":1079,"blockSize":1,"ivLength":16,"keyLength":32} | 0dc26e7a2d370d024f8e6f5534afcd5f8c2c57c6ce204126823c336a742f0434679bad9f478ed2856a10264f296d66d9fe71ec8a6c2480ffbe9a781f67c1 | true
aria-256-ecb | {"mode":"ecb","name":"aria-256-ecb","nid":1075,"blockSize":16,"keyLength":32} | cb6cb9191d46bf4c770b3595970bed6b71c83abef9456d2e4463545203f90a069d4c50a66192f07983a3a9eabbf1c012163b36ff488b0b212e4944d04a605d63 | true
aria-256-gcm | {"mode":"gcm","name":"aria-256-gcm","nid":1125,"blockSize":1,"ivLength":12,"keyLength":32} | 7278c8c2127455b95ce983dfb388e33ee2b7315762be46f6792522e97a00527f31377c45d17156315fe6862a965f83aa2a0e24cf60e7e298a8aa2bea5b4b | 2cfbf1bca5cb834c20038db1df0671b7 | true
aria-256-ofb | {"mode":"ofb","name":"aria-256-ofb","nid":1078,"blockSize":1,"ivLength":16,"keyLength":32} | 0dc26e7a2d370d024f8e6f5534afcd5fd9c5e714893fbb0faa46e29395a41fc35c206935ea4e26efda76275ce8637bf4f0c27c016ce865bb493ef0c3c571 | true
aria128 | {"mode":"cbc","name":"aria-128-cbc","nid":1066,"blockSize":16,"ivLength":16,"keyLength":16} | 7e1d84f79eae835ee9c0f2f8bebfa027a715542ba533a4a298d1bec5168729d67c71f92043a12912099ceb8a9de1dc29051ba31a0291077875b90d524ad4c6e3 | true
aria192 | {"mode":"cbc","name":"aria-192-cbc","nid":1071,"blockSize":16,"ivLength":16,"keyLength":24} | 814d1bca3abe0ace5abd660c102d17bb3196152e3f43a0ac4cec5afdf10f4fda8a385262cded390d015a2f03165f51aa19064e77b5249d94ed805a2d67455688 | true
aria256 | {"mode":"cbc","name":"aria-256-cbc","nid":1076,"blockSize":16,"ivLength":16,"keyLength":32} | da328e56b50ea49761de9d0d0d6b954788fde60c5b30f1febe149e71bf769832b332d95ed43aacd06aba67d24d9875b8121a8a6d043b9375cd4dce9301fc229b | true
camellia-128-cbc | {"mode":"cbc","name":"camellia-128-cbc","nid":751,"blockSize":16,"ivLength":16,"keyLength":16} | fedadba5e08b46a504dae48cb581b87fc3854d585ac653ea76f0bfb08499c0aa5fed53d2ada35a68418fc3eadc885bee0d1317606447b75b7efe3107b686d6eb | true
camellia-128-cfb | {"mode":"cfb","name":"camellia-128-cfb","nid":757,"blockSize":1,"ivLength":16,"keyLength":16} | 025c2c62c2b54baf7c2f975f6fca39671f25f754e5e66913afbec105cd4851ede873781a33b71ea584333e0ab7376afd9ed7c259cb65f8a0a891e4ed2b84 | true
camellia-128-cfb1 | {"mode":"cfb","name":"camellia-128-cfb1","nid":760,"blockSize":1,"ivLength":16,"keyLength":16} | 0d2e39060f2a659ffea9073d02d9966ca01ca3e1700f6a8fd458d485df77c6a858e4e61f7509cbda8704151e69f70b861a502396c4c917bb7bf4faa13f06 | true
camellia-128-cfb8 | {"mode":"cfb","name":"camellia-128-cfb8","nid":763,"blockSize":1,"ivLength":16,"keyLength":16} | 0242fa6a87d3ee86ff0f56024497ffe7dbab8344ed6279166e08590483d8d5db4203c9ea8e274e0f56282269c580dba36e26869e900263967c4949b4f650 | true
camellia-128-ctr | {"mode":"ctr","name":"camellia-128-ctr","nid":963,"blockSize":1,"ivLength":16,"keyLength":16} | 025c2c62c2b54baf7c2f975f6fca39671165deaecadb51e24ae272afa79122773f1216365b1f0b28984e727473c80f9370f96de058f5b761aeeb1f09b49a | true
camellia-128-ecb | {"mode":"ecb","name":"camellia-128-ecb","nid":754,"blockSize":16,"keyLength":16} | 85a3942211d3ae2fcb4a724ab2bf8aa7de8a09e120872c0e36db6bef38c817302202f5a28557b27010528382aacc8f60eba3650c84812aab9a40bfb39de865e3 | true
camellia-128-ofb | {"mode":"ofb","name":"camellia-128-ofb","nid":766,"blockSize":1,"ivLength":16,"keyLength":16} | 025c2c62c2b54baf7c2f975f6fca39679653be79f09192ede909b82174b221636cf9c1cd550b66a9d3ec952a566560bc5730e8ad9c7a0c4889dbe672302d | true
camellia-192-cbc | {"mode":"cbc","name":"camellia-192-cbc","nid":752,"blockSize":16,"ivLength":16,"keyLength":24} | be0e25207aa34f7128dc59223f5687c2cc3b78a314de219ebb575fab0e970432bc17dd2f01a9627ee0796c53c6b1280fa91a1688ac63196238b206e1e7ae0bfa | true
camellia-192-cfb | {"mode":"cfb","name":"camellia-192-cfb","nid":758,"blockSize":1,"ivLength":16,"keyLength":24} | df5a3c3ee925da843db2b27c2977cf67c84d3a6dc13dcc09dfc13443acaa61d068e9635fda98bc3db23b1dad5ea3b8869b3d7838a512ed1c1d69c6018702 | true
camellia-192-cfb1 | {"mode":"cfb","name":"camellia-192-cfb1","nid":761,"blockSize":1,"ivLength":16,"keyLength":24} | a83815d889d014d4a4c19d78e51d88aa29e582e76b6960825a43c47f4d80b6b4f31cf08b3f1b462946b9e64e0124ef797b8a79de0502d8045761c30aea7a | true
camellia-192-cfb8 | {"mode":"cfb","name":"camellia-192-cfb8","nid":764,"blockSize":1,"ivLength":16,"keyLength":24} | dfc87744ba613b2cdb5824f5a2ed7e50a4357d21299dfc6e792fea2bca8da0fb996a796f3049b47dd66876c173967cb154d7067810c95988608897cdbd83 | true
camellia-192-ctr | {"mode":"ctr","name":"camellia-192-ctr","nid":967,"blockSize":1,"ivLength":16,"keyLength":24} | df5a3c3ee925da843db2b27c2977cf676a09d73b7cefb778ead0f5685ea83f6702609825d21649187cd5368dbbef50274dc9fc270133527887a1bbe73009 | true
camellia-192-ecb | {"mode":"ecb","name":"camellia-192-ecb","nid":755,"blockSize":16,"keyLength":24} | b55064a4507b00070260c222d87760e63d44af30322736ad4de47dfa8e51d90262a8b09d1a09ca2f5a1ad6940aa9cb22f08af4b77d7334d993c80052e01ef05c | true
camellia-192-ofb | {"mode":"ofb","name":"camellia-192-ofb","nid":767,"blockSize":1,"ivLength":16,"keyLength":24} | df5a3c3ee925da843db2b27c2977cf67ef2366e08bd03cbc1f7823fecf41d252e479fe9c81002f864fc50eb72a1c877cf038fc65390901408581430a5250 | true
camellia-256-cbc | {"mode":"cbc","name":"camellia-256-cbc","nid":753,"blockSize":16,"ivLength":16,"keyLength":32} | 0a08f6387b0dbf314fd29ead88ffa7d6f07eb9c24c7b3c5afa2952a10b16122597edc7afdeeed0aab59f4680620710c1084cd5b5b0a834d121e2f4f8141cc9f4 | true
camellia-256-cfb | {"mode":"cfb","name":"camellia-256-cfb","nid":759,"blockSize":1,"ivLength":16,"keyLength":32} | 8e2dc7aced67e6c6a0057e2087d7a3d6a12e57fcf31801073f1be5b9a3033cc30d1e93c33392500f46537e82eb7bb228e4d4c2b72d6e84e42007b36d4428 | true
camellia-256-cfb1 | {"mode":"cfb","name":"camellia-256-cfb1","nid":762,"blockSize":1,"ivLength":16,"keyLength":32} | 8c172ab1fae694fde04da58db00dd4e2802c936d9cc80b2673ad8078e6ca291975dd7962edfddf157525aa3acb0205695989de01882f7138d7e41f60ab5e | true
camellia-256-cfb8 | {"mode":"cfb","name":"camellia-256-cfb8","nid":765,"blockSize":1,"ivLength":16,"keyLength":32} | 8ea26da8e85aabe89791822e1608d9a646e6eb949de3b14923c34f62dd3a350ab9da80733453e8e2677781880f8ed82962e03022c42bac74310bdc44ae9a | true
camellia-256-ctr | {"mode":"ctr","name":"camellia-256-ctr","nid":971,"blockSize":1,"ivLength":16,"keyLength":32} | 8e2dc7aced67e6c6a0057e2087d7a3d6cf3c2ecfdd193543beea54d5befa2a1cd1102f259d15f2fafaa6371a6054d37051c9ab142e6204588cd09d103c36 | true
camellia-256-ecb | {"mode":"ecb","name":"camellia-256-ecb","nid":756,"blockSize":16,"keyLength":32} | 9fe8ee7219d9aff8dade5fd9e4d73caa11ba51fdab5f1202424c8a266bc7f4b99e920333b2cdd8988000ee01c852b344a67415267ad79bcde96c94f3f1bdc22b | true
camellia-256-ofb | {"mode":"ofb","name":"camellia-256-ofb","nid":768,"blockSize":1,"ivLength":16,"keyLength":32} | 8e2dc7aced67e6c6a0057e2087d7a3d6efa5257f6ab1d69a61afa1963b21232a3513a13984178e656cc9319a9819e241908185aa67cf1bdfb954c8dc0199 | true
camellia128 | {"mode":"cbc","name":"camellia-128-cbc","nid":751,"blockSize":16,"ivLength":16,"keyLength":16} | fedadba5e08b46a504dae48cb581b87fc3854d585ac653ea76f0bfb08499c0aa5fed53d2ada35a68418fc3eadc885bee0d1317606447b75b7efe3107b686d6eb | true
camellia192 | {"mode":"cbc","name":"camellia-192-cbc","nid":752,"blockSize":16,"ivLength":16,"keyLength":24} | be0e25207aa34f7128dc59223f5687c2cc3b78a314de219ebb575fab0e970432bc17dd2f01a9627ee0796c53c6b1280fa91a1688ac63196238b206e1e7ae0bfa | true
camellia256 | {"mode":"cbc","name":"camellia-256-cbc","nid":753,"blockSize":16,"ivLength":16,"keyLength":32} | 0a08f6387b0dbf314fd29ead88ffa7d6f07eb9c24c7b3c5afa2952a10b16122597edc7afdeeed0aab59f4680620710c1084cd5b5b0a834d121e2f4f8141cc9f4 | true
chacha20 | {"mode":"stream","name":"chacha20","nid":1019,"ivLength":16,"keyLength":32} | 5c0c65b6527b2e966c54eb17d748759b6c5ff6f7dcbf54301f6d7a944b01560208b3e782d996ce65237807777dab0883e948fd8f443fad706490e5a9a08b | true
chacha20-poly1305 | {"mode":"stream","name":"chacha20-poly1305","nid":1018,"ivLength":12,"keyLength":32} | 015e9e30e12f848ef2bc122636295161059c40b58623d31813fd7531ece22d787cd6622f0ee7c326ad6148e154c9872a1f6ee173cf5747ebe3401a1344e7 | 012269d34b5339a4c51186fbc36e4413 | true
des-ede | {"mode":"ecb","name":"des-ede","nid":32,"blockSize":8,"keyLength":16} | a615369f34cdd941e82b0a2774674868920005a614b6f8c37bd8d32660f1244c9323519834703bab8e91f9613bcde9748c84cbc28b2f23b8b8cf2ba2c71042e7 | true
des-ede-cbc | {"mode":"cbc","name":"des-ede-cbc","nid":43,"blockSize":8,"ivLength":8,"keyLength":16} | 4f506f7e6af64b180fb81cfed418b821a6843820242c4f4d31ea708935d250efcbdecf844853a733e82c165ff6b8d4e082c85de1253a4bcacbd4f0f8ae6e7564 | true
des-ede-cfb | {"mode":"cfb","name":"des-ede-cfb","nid":60,"blockSize":1,"ivLength":8,"keyLength":16} | f821ac90980deb3b3f4098c297070e0b01fb776dc69ca3aa6c63e07acaeda285e5f70a286275c11914e05c1561cef935a6855ee49247d385241b905a3fef | true
des-ede-ecb | {"mode":"ecb","name":"des-ede","nid":32,"blockSize":8,"keyLength":16} | a615369f34cdd941e82b0a2774674868920005a614b6f8c37bd8d32660f1244c9323519834703bab8e91f9613bcde9748c84cbc28b2f23b8b8cf2ba2c71042e7 | true
des-ede-ofb | {"mode":"ofb","name":"des-ede-ofb","nid":62,"blockSize":1,"ivLength":8,"keyLength":16} | f821ac90980deb3b64f4d2cbe43e5d3ff3b0b7a1e11d73559c579c9ef5b65470a327b9a3cc322a8f51bdba39b13a7a40d363140ab1b1beee59a00c35c0a0 | true
des-ede3 | {"mode":"ecb","name":"des-ede3","nid":33,"blockSize":8,"keyLength":24} | d4ce7da964c6ea7f056ded409cbf0c65df76dadab49d0ff7ff6560226f21310883bae2456e6928bd9928cc834e2cf2f79a22bc4739f11170a4da6a07015a9a4d | true
des-ede3-cbc | {"mode":"cbc","name":"des-ede3-cbc","nid":44,"blockSize":8,"ivLength":8,"keyLength":24} | 07872564f997bdaed70922c0a9bf864b376157cf7c58fe49d224f5cb14df3137d5246782d1a4cb905356d6738dd83d50b9579561a86b9faec40be84e3c21ac5e | true
des-ede3-cfb | {"mode":"cfb","name":"des-ede3-cfb","nid":61,"blockSize":1,"ivLength":8,"keyLength":24} | 6e62a156009076fd8c48d78b54a7f4a22cb0a2f7bada1defab60cd745b73c799588a865ea03367da09c10d23d4e7c28a60208458063ed122103c58f69f60 | true
des-ede3-cfb1 | {"mode":"cfb","name":"des-ede3-cfb1","nid":658,"blockSize":1,"ivLength":8,"keyLength":24} | 12df8385c03e7274455c04f15e31a32d1a46d57e377cdbe9c1ff8b007d4ff233e1fe6e9fda8db47570921edd7ca84df1d9cb1e0bcf15cbe1d7cc654b22a3 | true
des-ede3-cfb8 | {"mode":"cfb","name":"des-ede3-cfb8","nid":659,"blockSize":1,"ivLength":8,"keyLength":24} | 6eed27834fd551ef662df13bbcd2e00c54f2d7a0e980dc9d20a0ae73a3e8d7ba87db38f949e0bcea115ec4c934b6a61780a1d6b9d9ed16c3acb9c0c3b89b | true
des-ede3-ecb | {"mode":"ecb","name":"des-ede3","nid":33,"blockSize":8,"keyLength":24} | d4ce7da964c6ea7f056ded409cbf0c65df76dadab49d0ff7ff6560226f21310883bae2456e6928bd9928cc834e2cf2f79a22bc4739f11170a4da6a07015a9a4d | true
des-ede3-ofb | {"mode":"ofb","name":"des-ede3-ofb","nid":63,"blockSize":1,"ivLength":8,"keyLength":24} | 6e62a156009076fd327e5bcf90c3cb1a7fc0c7b590c121115e0365a4a941aed5fa595c014fb6853858b5a0553569c8a1af105bbf97f1196a4b9f43b29ffe | true
des3 | {"mode":"cbc","name":"des-ede3-cbc","nid":44,"blockSize":8,"ivLength":8,"keyLength":24} | 07872564f997bdaed70922c0a9bf864b376157cf7c58fe49d224f5cb14df3137d5246782d1a4cb905356d6738dd83d50b9579561a86b9faec40be84e3c21ac5e | true
des3-wrap | {"mode":"wrap","name":"id-smime-alg-cms3deswrap","nid":246,"blockSize":8,"keyLength":24} | 40 | true
id-aes128-CCM | {"mode":"ccm","name":"id-aes128-ccm","nid":896,"blockSize":1,"ivLength":12,"keyLength":16} | 11524cc9737ef6ef599ed129a3490c89e3fe7f71b0e60f66dc902b11dce69a291ffdf45976b47ecf97aea88f25bbfbe1837d49a6d45484b7ced6448c4717 | 50109353b4ba1db1039eae817dabcc81 | true
id-aes128-GCM | {"mode":"gcm","name":"id-aes128-gcm","nid":895,"blockSize":1,"ivLength":12,"keyLength":16} | 0cb9138bc8c288bfed96195d4453d49f6e95356b6a04c74cbda7a93ee12a480dac24b31ed9a4d2908f9dfdb77dba3a56cd4fe661c0fcb608cc67dd35b2ec | aad803e182b7709e34630adfb1fe0062 | true
id-aes128-wrap | {"mode":"wrap","name":"id-aes128-wrap","nid":788,"blockSize":8,"ivLength":8,"keyLength":16} | 32a77077f4766052d8f23cfaf82aee4f46fcdb7fe47c22de151a895b22f1c520 | true
id-aes128-wrap-pad | {"mode":"wrap","name":"id-aes128-wrap-pad","nid":897,"blockSize":8,"ivLength":4,"keyLength":16} | 84e25828f31d586af0afd8822c91cf0ec3b74a46aaad88ea117a8db423c02244 | true
id-aes192-CCM | {"mode":"ccm","name":"id-aes192-ccm","nid":899,"blockSize":1,"ivLength":12,"keyLength":24} | 140839928da4fc312e7c3cc9065cf189f5da76ff38724277f041e3bccb0a1bb86d43d1b8136a3df2cfab4c6d97b5e43fae99e951a0a2d859e172c74f1830 | 12646026ae3fa0a4f504446171627e18 | true
id-aes192-GCM | {"mode":"gcm","name":"id-aes192-gcm","nid":898,"blockSize":1,"ivLength":12,"keyLength":24} | 0f1818b077d843effb2a97cdf787d4cda8382c9695a5fe3bd2ea40a3abcaacc529400fc15cb6562f7a7fdb76da556a248586c0495b8fb21997d7b23cbe82 | 3fcea7282fba8f9e3aa2d30fbdb7dfeb | true
id-aes192-wrap | {"mode":"wrap","name":"id-aes192-wrap","nid":789,"blockSize":8,"ivLength":8,"keyLength":24} | 9cc8b6b457048148af60d37a7a8edd8131e5d3f8696c57ca65da324236e3c90a | true
id-aes192-wrap-pad | {"mode":"wrap","name":"id-aes192-wrap-pad","nid":900,"blockSize":8,"ivLength":4,"keyLength":24} | 69794ac36318df8692c68d46e366ec89f14c2b7ccd7185b1158bcb74470457f6 | true
id-aes256-CCM | {"mode":"ccm","name":"id-aes256-ccm","nid":902,"blockSize":1,"ivLength":12,"keyLength":32} | 0af54062d614e6bd5f76189981930313bc572d3d653a5c663030c5811995765107e88f8a69052e53c81b44ba88620b05d34ee18b4d06cc004be8cd590a05 | 3e245de87a90f3b74c5a3b9d6a99706b | true
id-aes256-GCM | {"mode":"gcm","name":"id-aes256-gcm","nid":901,"blockSize":1,"ivLength":12,"keyLength":32} | 68c51b8f9c603f29b8c7d3dde2d5461f74f1cfdae1487a0a532520c55ba20fb14f3b7573f17fe1d143e5dcf49808e850b32f07bb57cdf2cf0526810659c3 | f073671c740a17f5b371328a5a1dfcca | true
id-aes256-wrap | {"mode":"wrap","name":"id-aes256-wrap","nid":790,"blockSize":8,"ivLength":8,"keyLength":32} | ba504a5d4f052cc0da8f506d881375c2a1b085ff10e82b9c25f5d76c350223e8 | true
id-aes256-wrap-pad | {"mode":"wrap","name":"id-aes256-wrap-pad","nid":903,"blockSize":8,"ivLength":4,"keyLength":32} | bc61761a1d71930c19af652b312769fb7e2bbf3b82e039a9a372926bdae2b62b | true
id-smime-alg-CMS3DESwrap | {"mode":"wrap","name":"id-smime-alg-cms3deswrap","nid":246,"blockSize":8,"keyLength":24} | 40 | true
sm4 | {"mode":"cbc","name":"sm4-cbc","nid":1134,"blockSize":16,"ivLength":16,"keyLength":16} | c4b494ad53715afd852c689134e6094fc92a8d38e8309098e6188662a0cd0d9dddd63ff9f14926ce0ef43e12e4f18054e1c83cc3d61c9bcbb75e70239ecb5737 | true
sm4-cbc | {"mode":"cbc","name":"sm4-cbc","nid":1134,"blockSize":16,"ivLength":16,"keyLength":16} | c4b494ad53715afd852c689134e6094fc92a8d38e8309098e6188662a0cd0d9dddd63ff9f14926ce0ef43e12e4f18054e1c83cc3d61c9bcbb75e70239ecb5737 | true
sm4-cfb | {"mode":"cfb","name":"sm4-cfb","nid":1137,"blockSize":1,"ivLength":16,"keyLength":16} | 6d321a1de89dca560d22436a86e84b139b6b88745cbd76b1365ae903ce5d998fdf8f5a632c8b9ca9dbf26a6dbb8906cfc18b7cafa82e4272f88ac40aada2 | true
sm4-ctr | {"mode":"ctr","name":"sm4-ctr","nid":1139,"blockSize":1,"ivLength":16,"keyLength":16} | 6d321a1de89dca560d22436a86e84b1373b1b7047dfc255ce609d869f8545e90e64c7968101c1d711cff270aac08b44230015bb2b8a2fb819a0dba27c13b | true
sm4-ecb | {"mode":"ecb","name":"sm4-ecb","nid":1133,"blockSize":16,"keyLength":16} | f6bd558d82c70e2bf6793a149279330b71e26ff47b5728b1d69de73544b61cd27bca7e7158394f0906bab2b8c129572ce8f660cff3e3410e90b6a910c753c394 | true
sm4-ofb | {"mode":"ofb","name":"sm4-ofb","nid":1135,"blockSize":1,"ivLength":16,"keyLength":16} | 6d321a1de89dca560d22436a86e84b1383bf8e73094aca0efb9118bef5e91fb0d5deeaa851b360472b4132b4264986e61f58d272f3a7eb50da8c0f525961 | true
"###;

const MODES: &str = r###"const crypto = require("crypto");
const k16 = Buffer.alloc(16, 1), k32 = Buffer.alloc(32).map((_, i) => i);
const t = (label, f) => { try { const r = f(); console.log(label, r === undefined ? "ok" : Buffer.isBuffer(r) ? r.toString("hex") : r); } catch (e) { console.log(label, "ERR", e.name, e.code, e.message); } };
t("ccm no tag", () => crypto.createCipheriv("aes-128-ccm", k16, Buffer.alloc(12)));
t("ccm bad iv 6", () => crypto.createCipheriv("aes-128-ccm", k16, Buffer.alloc(6), { authTagLength: 16 }));
t("ccm iv 13", () => crypto.createCipheriv("aes-128-ccm", k16, Buffer.alloc(13), { authTagLength: 16 }) && undefined);
t("ccm iv 7", () => crypto.createCipheriv("aes-128-ccm", k16, Buffer.alloc(7), { authTagLength: 16 }) && undefined);
t("ccm tag 5", () => crypto.createCipheriv("aes-128-ccm", k16, Buffer.alloc(12), { authTagLength: 5 }));
t("ccm aad nolen", () => crypto.createCipheriv("aes-128-ccm", k16, Buffer.alloc(12), { authTagLength: 16 }).setAAD(Buffer.from("x")) && undefined);
t("ccm twice", () => { const c = crypto.createCipheriv("aes-128-ccm", k16, Buffer.alloc(12), { authTagLength: 8 }); c.setAAD(Buffer.from("x"), { plaintextLength: 3 }); c.update("abc"); return c.update("d"); });
t("ccm wrong len", () => { const c = crypto.createCipheriv("aes-128-ccm", k16, Buffer.alloc(12), { authTagLength: 8 }); c.setAAD(Buffer.from("x"), { plaintextLength: 3 }); return c.update("abcd"); });
t("ccm no aad", () => { const c = crypto.createCipheriv("aes-128-ccm", k16, Buffer.alloc(12), { authTagLength: 8 }); const o = Buffer.concat([c.update("abcd"), c.final()]); return o.toString("hex") + " " + c.getAuthTag().toString("hex"); });
t("ccm empty", () => { const c = crypto.createCipheriv("aes-128-ccm", k16, Buffer.alloc(11), { authTagLength: 12 }); const o = c.final(); return o.toString("hex") + " " + c.getAuthTag().toString("hex"); });
t("ccm dec bad", () => { const d = crypto.createDecipheriv("aes-128-ccm", k16, Buffer.alloc(12), { authTagLength: 8 }); d.setAuthTag(Buffer.alloc(8)); const o = d.update(Buffer.from("abcd")); console.log(" upd", o.length); return d.final(); });
t("ccm dec notag", () => { const d = crypto.createDecipheriv("aes-128-ccm", k16, Buffer.alloc(12), { authTagLength: 8 }); return d.update(Buffer.from("abcd")); });
t("ccm empty upd", () => { const c = crypto.createCipheriv("aes-128-ccm", k16, Buffer.alloc(11), { authTagLength: 12 }); c.update(""); const o = c.final(); return o.toString("hex") + " " + c.getAuthTag().toString("hex"); });
t("wrappad 3", () => { const c = crypto.createCipheriv("id-aes128-wrap-pad", k16, Buffer.alloc(4, 2)); const w = Buffer.concat([c.update(Buffer.alloc(3, 7)), c.final()]); const d = crypto.createDecipheriv("id-aes128-wrap-pad", k16, Buffer.alloc(4, 2)); return w.toString("hex") + " " + Buffer.concat([d.update(w), d.final()]).toString("hex"); });
t("unwrappad bad", () => { const d = crypto.createDecipheriv("id-aes128-wrap-pad", k16, Buffer.alloc(4, 2)); return d.update(Buffer.alloc(16)); });
t("ocb tag 8", () => { const c = crypto.createCipheriv("aes-128-ocb", k16, Buffer.alloc(12), { authTagLength: 8 }); c.setAAD(Buffer.from("hdr")); const o = Buffer.concat([c.update("0123456789abcdefXYZ"), c.final()]); return o.toString("hex") + " " + c.getAuthTag().toString("hex"); });
t("ocb notag", () => { const c = crypto.createCipheriv("aes-128-ocb", k16, Buffer.alloc(12)); const o = Buffer.concat([c.update("hello"), c.final()]); return o.toString("hex") + " " + c.getAuthTag().toString("hex"); });
t("ocb upd", () => { const c = crypto.createCipheriv("aes-128-ocb", k16, Buffer.alloc(12), { authTagLength: 16 }); return [c.update("0123456789abcdefXYZ").length, c.update("0123456789abcd").length, c.final().length].join(); });
t("ocb iv 15", () => { const c = crypto.createCipheriv("aes-128-ocb", k16, Buffer.alloc(15, 3), { authTagLength: 16 }); const o = Buffer.concat([c.update("0123456789abcdef0123456789abcdef!"), c.final()]); return o.toString("hex") + " " + c.getAuthTag().toString("hex"); });
t("ocb iv 16", () => crypto.createCipheriv("aes-128-ocb", k16, Buffer.alloc(16), { authTagLength: 16 }));
t("ocb iv 0", () => crypto.createCipheriv("aes-128-ocb", k16, Buffer.alloc(0), { authTagLength: 16 }));
t("ocb dec bad", () => { const d = crypto.createDecipheriv("aes-128-ocb", k16, Buffer.alloc(12), { authTagLength: 16 }); d.setAuthTag(Buffer.alloc(16)); d.update(Buffer.from("abcd")); return d.final(); });
t("xts short", () => { const c = crypto.createCipheriv("aes-128-xts", k32, Buffer.alloc(16)); return c.update(Buffer.alloc(15)); });
t("xts dup", () => { const c = crypto.createCipheriv("aes-128-xts", Buffer.alloc(32, 5), Buffer.alloc(16)); return c.update(Buffer.alloc(16)); });
t("xts two", () => { const c = crypto.createCipheriv("aes-128-xts", k32, Buffer.alloc(16, 9)); return Buffer.concat([c.update(Buffer.alloc(17, 1)), c.update(Buffer.alloc(16, 2)), c.final()]); });
t("wrap bad", () => { const c = crypto.createCipheriv("id-aes128-wrap", k16, Buffer.alloc(8, 0xa6)); return c.update(Buffer.alloc(12)); });
t("wrap 8", () => { const c = crypto.createCipheriv("id-aes128-wrap", k16, Buffer.alloc(8, 0xa6)); return c.update(Buffer.alloc(8)); });
t("wrap iv7", () => { const c = crypto.createCipheriv("id-aes128-wrap", k16, Buffer.alloc(7)); return c.update(Buffer.alloc(16)); });
t("wrap noiv", () => { const c = crypto.createCipheriv("id-aes128-wrap", k16, null); return c.update(Buffer.alloc(16)); });
t("wrappad noiv", () => { const c = crypto.createCipheriv("id-aes128-wrap-pad", k16, null); return Buffer.concat([c.update(Buffer.alloc(5, 7)), c.final()]); });
t("wrappad 8", () => { const c = crypto.createCipheriv("id-aes128-wrap-pad", k16, Buffer.from("a65959a6", "hex")); return Buffer.concat([c.update(Buffer.alloc(8, 7)), c.final()]); });
t("unwrap bad", () => { const c = crypto.createDecipheriv("id-aes128-wrap", k16, Buffer.alloc(8, 0xa6)); return c.update(Buffer.alloc(24)); });
t("wrap two", () => { const c = crypto.createCipheriv("id-aes128-wrap", k16, Buffer.alloc(8, 1)); return Buffer.concat([c.update(Buffer.alloc(16)), c.update(Buffer.alloc(16, 1)), c.final()]); });
t("3deswrap dec", () => { const k = Buffer.alloc(24).map((_, i) => i * 3 + 1); const c = crypto.createCipheriv("des3-wrap", k, null); const w = c.update(Buffer.alloc(24, 0x42)); const d = crypto.createDecipheriv("des3-wrap", k, null); return w.length + " " + d.update(w).toString("hex"); });
t("3deswrap bad", () => { const k = Buffer.alloc(24).map((_, i) => i * 3 + 1); const d = crypto.createDecipheriv("des3-wrap", k, null); return d.update(Buffer.alloc(40)); });
t("hmac final", () => { const c = crypto.createCipheriv("aes-128-cbc-hmac-sha1", k16, Buffer.alloc(16)); return c.final(); });
t("cfb nopad", () => { const c = crypto.createCipheriv("aes-128-cfb", k16, Buffer.alloc(16)); c.setAutoPadding(false); return Buffer.concat([c.update("abc"), c.final()]); });
t("ecb des-ede", () => { const c = crypto.createCipheriv("des-ede", k16, null); return Buffer.concat([c.update("abc"), c.final()]); });
t("bf", () => crypto.createCipheriv("bf-cbc", k16, Buffer.alloc(8)));
t("des-cbc", () => crypto.createCipheriv("des-cbc", Buffer.alloc(8), Buffer.alloc(8)));
t("gcm iv 1", () => { const c = crypto.createCipheriv("aria-128-gcm", k16, Buffer.alloc(1)); c.setAAD(Buffer.from("a")); const o = Buffer.concat([c.update("xyz"), c.final()]); return o.toString("hex") + " " + c.getAuthTag().toString("hex"); });
t("info", () => JSON.stringify([crypto.getCipherInfo("aes-128-ccm", { ivLength: 7 }), crypto.getCipherInfo("aes-128-ccm", { ivLength: 14 }), crypto.getCipherInfo("aes-128-ocb", { ivLength: 15 }), crypto.getCipherInfo("aes-128-gcm", { ivLength: 1 }), crypto.getCipherInfo("aes-128-cbc", { ivLength: 15 }), crypto.getCipherInfo("aes-128-cbc", { keyLength: 16 }), crypto.getCipherInfo("chacha20-poly1305", { ivLength: 8 }), crypto.getCipherInfo("nope")]));
"###;

const MODES_EXPECTED: &str = r###"ccm no tag ERR TypeError ERR_CRYPTO_INVALID_AUTH_TAG authTagLength required for aes-128-ccm
ccm bad iv 6 ERR TypeError ERR_CRYPTO_INVALID_IV Invalid initialization vector
ccm iv 13 ok
ccm iv 7 ok
ccm tag 5 ERR TypeError ERR_CRYPTO_INVALID_AUTH_TAG Invalid authentication tag length: 5
ccm aad nolen ERR TypeError ERR_MISSING_ARGS options.plaintextLength required for CCM mode with AAD
ccm twice ERR Error undefined Trying to add data in unsupported state
ccm wrong len ERR Error undefined Trying to add data in unsupported state
ccm no aad 6ed00431 b408b179cc04001f
ccm empty ERR Error ERR_OSSL_TAG_NOT_SET error:1C800077:Provider routines::tag not set
 upd 0
ccm dec bad ERR Error undefined Unsupported state or unable to authenticate data
ccm dec notag 
ccm empty upd  3079e023ccfc52a635def147
wrappad 3 8d140b842f830d45b503954a26e8b34f 070707
unwrappad bad ERR Error undefined Trying to add data in unsupported state
ocb tag 8 36eedda785c19f7e6a5a703eaf6a684205d07c e25bb1cf663523c3
ocb notag ERR TypeError ERR_CRYPTO_INVALID_AUTH_TAG authTagLength required for aes-128-ocb
ocb upd 16,16,1
ocb iv 15 29494dbf5018dcb77a6a18b29d2dfb29dd1ab481934d7132b64661971778c3b559 9113e21a6c310ff8449d3baacd242510
ocb iv 16 ERR TypeError ERR_CRYPTO_INVALID_IV Invalid initialization vector
ocb iv 0 ERR TypeError ERR_CRYPTO_INVALID_IV Invalid initialization vector
ocb dec bad ERR Error undefined Unsupported state or unable to authenticate data
xts short ERR Error undefined Trying to add data in unsupported state
xts dup ERR Error ERR_OSSL_XTS_DUPLICATED_KEYS error:1C800095:Provider routines::xts duplicated keys
xts two ec8a0fb94dd24cb83df57c2dd5db9c4b3f7729a3a68fde28afe18fe6a7a19d82a0
wrap bad ERR Error undefined Trying to add data in unsupported state
wrap 8 ERR Error undefined Trying to add data in unsupported state
wrap iv7 ERR TypeError ERR_CRYPTO_INVALID_IV Invalid initialization vector
wrap noiv ERR TypeError ERR_CRYPTO_INVALID_IV Invalid initialization vector
wrappad noiv ERR TypeError ERR_CRYPTO_INVALID_IV Invalid initialization vector
wrappad 8 ef1d6a8a0f2f4108857effda6109f258
unwrap bad ERR Error undefined Trying to add data in unsupported state
wrap two 7f47c82d3f270aabeed883ad132bb0bf958f761512936d50444b1fb7bb60c5685facddc6a71d916d89717aa4a343001e
3deswrap dec 40 424242424242424242424242424242424242424242424242
3deswrap bad ERR Error undefined Trying to add data in unsupported state
hmac final 
cfb nopad d7cccc
ecb des-ede c576dd250e9f3ba1
bf ERR Error ERR_OSSL_EVP_UNSUPPORTED error:0308010C:digital envelope routines::unsupported
des-cbc ERR Error ERR_OSSL_EVP_UNSUPPORTED error:0308010C:digital envelope routines::unsupported
gcm iv 1 2f0905 91a400c0d8097ca3b3e613e9a4ff5915
info [{"mode":"ccm","name":"id-aes128-ccm","nid":896,"blockSize":1,"ivLength":7,"keyLength":16},null,{"mode":"ocb","name":"aes-128-ocb","nid":958,"blockSize":16,"ivLength":15,"keyLength":16},{"mode":"gcm","name":"id-aes128-gcm","nid":895,"blockSize":1,"ivLength":1,"keyLength":16},null,{"mode":"cbc","name":"aes-128-cbc","nid":419,"blockSize":16,"ivLength":16,"keyLength":16},null,null]
"###;

const HASHES: &str = r###"const c=require("crypto");
for (const [a,o] of [["shake256",undefined],["shake128",undefined],["shake256",{outputLength:5}],["sha256",{outputLength:32}],["sha256",{outputLength:10}],["md5-sha1"],["blake2b512"],["blake2s256"],["sm3"],["ripemd"],["rmd160"],["RSA-RIPEMD160"],["sha3-256"],["sha3-224"],["sha3-384"],["sha3-512"],["md4"],["RSA-SHA3-256"],["id-rsassa-pkcs1-v1_5-with-sha3-512"],["ssl3-md5"],["RSA-SHA1-2"],["sha512-224WithRSAEncryption"],["shake128",{outputLength:0}],["nope"],["shake128",{outputLength:-1}],["shake128",{outputLength:1.5}],["shake128",{outputLength:"3"}]]) { try { console.log(a, c.createHash(a,o).update("abc").digest("hex")); } catch(e) { console.log(a, "ERR", e.name, e.code, e.message); } }
for (const a of ["sha3-256","blake2b512","blake2s256","ripemd160","sm3","md5-sha1","shake128","sha512-224","nope"]) { try { console.log("hmac", a, c.createHmac(a,"key").update("abc").digest("hex"), c.createHmac(a, "k".repeat(200)).update("x").digest("base64")); } catch(e) { console.log("hmac",a,"ERR", e.name, e.code, e.message); } }
for (const a of ["sha3-256","blake2b512","ripemd160","shake256","sm3","sha256"]) {
  try { console.log("pbkdf2", a, c.pbkdf2Sync("pw","salt",3,70,a).toString("hex")); } catch(e) { console.log("pbkdf2",a,"ERR", e.name, e.code, e.message); }
  try { console.log("hkdf", a, Buffer.from(c.hkdfSync(a,"key","salt","info",50)).toString("hex")); } catch(e) { console.log("hkdf",a,"ERR", e.name, e.code, e.message); }
}
const h=c.createHash("shake256",{outputLength:8}); h.update("a"); const h2=h.copy({outputLength:4}); h2.update("b"); console.log(h.digest("hex"), h2.digest("hex"));
try { h.update("x"); } catch (e) { console.log(e.code, e.message); }
try { h.copy(); } catch (e) { console.log(e.code, e.message); }
try { c.createHash("sha256").copy({outputLength: 5}); } catch (e) { console.log(e.code, e.message); }
console.log(c.hash("sha3-512","x"), c.hash("shake128","x"), c.hash("sha1", "abc", "base64"));
const s = c.createHash("sha3-256"); s.write("ab"); s.end("c"); s.on("data", (d) => console.log("stream", d.toString("hex")));
"###;

const HASHES_EXPECTED: &str = r###"shake256 483366601360a8771c6863080cc4114d8db44530f8f1e1ee4f94ea37e78b5739
shake128 5881092dd818bf5cf8a3ddb793fbcba7
shake256 4833666013
sha256 ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
sha256 ERR Error ERR_OSSL_EVP_NOT_XOF_OR_INVALID_LENGTH error:030000B2:digital envelope routines::not XOF or invalid length
md5-sha1 900150983cd24fb0d6963f7d28e17f72a9993e364706816aba3e25717850c26c9cd0d89d
blake2b512 ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d17d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923
blake2s256 508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982
sm3 66c7f0f462eeedd9d1f2d46bdc10e4e24167c4875cf2f7a2297da02b8f4ba8e0
ripemd 8eb208f7e05d987a9b044a8e98c6b087f15a0bfc
rmd160 8eb208f7e05d987a9b044a8e98c6b087f15a0bfc
RSA-RIPEMD160 8eb208f7e05d987a9b044a8e98c6b087f15a0bfc
sha3-256 3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532
sha3-224 e642824c3f8cf24ad09234ee7d3c766fc9a3a5168d0c94ad73b46fdf
sha3-384 ec01498288516fc926459f58e2c6ad8df9b473cb0fc08c2596da7cf0e49be4b298d88cea927ac7f539f1edf228376d25
sha3-512 b751850b1a57168a5693cd924b6b096e08f621827444f70d884f5d0240d2712e10e116e9192af3c91a7ec57647e3934057340b4cf408d5a56592f8274eec53f0
md4 ERR Error ERR_OSSL_EVP_UNSUPPORTED error:0308010C:digital envelope routines::unsupported
RSA-SHA3-256 3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532
id-rsassa-pkcs1-v1_5-with-sha3-512 b751850b1a57168a5693cd924b6b096e08f621827444f70d884f5d0240d2712e10e116e9192af3c91a7ec57647e3934057340b4cf408d5a56592f8274eec53f0
ssl3-md5 900150983cd24fb0d6963f7d28e17f72
RSA-SHA1-2 a9993e364706816aba3e25717850c26c9cd0d89d
sha512-224WithRSAEncryption 4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa
shake128 
nope ERR Error undefined Digest method not supported
shake128 ERR RangeError ERR_OUT_OF_RANGE The value of "options.outputLength" is out of range. It must be >= 0 && <= 4294967295. Received -1
shake128 ERR RangeError ERR_OUT_OF_RANGE The value of "options.outputLength" is out of range. It must be an integer. Received 1.5
shake128 ERR TypeError ERR_INVALID_ARG_TYPE The "options.outputLength" property must be of type number. Received type string ('3')
hmac sha3-256 09b6dbab8d11795ca7c8d82f1cf91682013c7cb980abbb25473be4ae7f7b5683 AKCHq04Fl8IL/Wkw8JpD/t0+HFRV0h/MaKi5avzKUB0=
hmac blake2b512 05cc4815438d5cfe68fff446b8df57828cc96189de4b4e928e3f06d815d64e5bc15124a02ffd39859b3e2476da03bc0235ca86df623af2a5631779809e9fd04a bVf4Osotc3zm5+etRuuDHiVoKBS5CnZdCUnBBR8It5SnwfuUx7o4jAZr/Lfv7Z2Le5HJBCdbx5J7haNvtF8OPg==
hmac blake2s256 0018bfddc0c878b3e270c6e59b35af01e164a42d89499c9447a90b58a4d60eb3 DMSbBpjS3VWSPMxYpOyTLdvlMcnfWpHWTNqGyNI8HBs=
hmac ripemd160 67fdce738ebfc7372bcd38f03c023b5746724d18 Dnh77QFBDlBy3rSYHeTBekrXZsQ=
hmac sm3 28e63256e7c5a087b1f073265dc53092163f7b82729735d06f28f10af9d52393 ubf6jYHdrxAv08M/jUO2yEfBXGLXFhTuhhlS891Wp0I=
hmac md5-sha1 a76e5cc4bdaa99b87cc7de69e0606e7d74fd2771909f120bb3b9a4649bb99bea2b4cec32 /A58hdinxXJ6aG2p9KJenDqJ1x2iEYu54XXlV7WYwCL92XQm
hmac shake128 ERR Error undefined error:00000000:lib(0)::reason(0)
hmac sha512-224 28bc0433ef684ea9877c2f2437aa0ccfc2e8eb3a9b4c731606877b4f Bp2zPhL9ySeBPFcvoUjByXjmqCt6xQS5m1ohqg==
hmac nope ERR TypeError ERR_CRYPTO_INVALID_DIGEST Invalid digest: nope
pbkdf2 sha3-256 aed9faa5b67ac0547e5e2d5de5354f5b2be85356663c59acd5fc0e8105a9552882dfed84c71f4a7ebc30890050ba445cbc19372821b3af30f0a9c28bd931716f04e28b835463
hkdf sha3-256 7c0ef77d81593534a377e29e61bed4e29ffad92bfced4a2521022782597b4554fec714c1f4c7ffce556cdd5f64a12b679074
pbkdf2 blake2b512 b8af03a148b71b1d13a52b37123c76c916b09f084879c57353999b4f9954b9efc1e92fd9b8f4f02af58bfaaf1674d136064b605f878c79d04fc26f06dc518b93cfcc91a43939
hkdf blake2b512 b3b27a54dac96f4819e8befbc643bd78b7882208b2e366043cbf47ab44b033a479f5293563d81628f93d30df7ece2024ff1a
pbkdf2 ripemd160 872082fba2e5e614e84d9290769acc835cdf6bf33bfa77845b62947255cd4aec63aeaeb39b5c379b43468b68813c1d633576c15967ebad459598293c134be6b0ef71c68298df
hkdf ripemd160 e9a427467d62f3ede3bc37f37451e1d65cfbb54248cda74eb1992e9677bb4e5ef48e1e9614a11a9cb8f06abf9db87d407364
pbkdf2 shake256 ERR Error undefined Deriving bits failed
hkdf shake256 ERR Error undefined Deriving bits failed
pbkdf2 sm3 76b8bd7bfd8c44e27165213ea16a2d81584a73555901d5757ef446b9d3c133e60167affc1568f8c2ab08c2fd37c6efc193249edd51e4eb64bf7f743714c66922c5a09078b37d
hkdf sm3 ad1e679b43f4eea80ab4b62d4f55fa9db9a1c148b199a15b3b1c47a3e95c37521adf1fd2b2446e47adcb5fa386f29c78a82f
pbkdf2 sha256 3c671234e386cb307220d70ee15c4985160b8a8dba62f247001b07974ad52ed1785bf34b5aa5dd58858f23f5b5c57ccf97e1e5af0752abba7151256f3eaa935a29361ced2832
hkdf sha256 9ca0d662557439e3b83365f2da4626d35da195c6d9d1779f09838cf9e408966ece99106e2585ace6f083fa1acd5192f430fd
867e2cb04f5a04dc effb6ac2
ERR_CRYPTO_HASH_FINALIZED Digest already called
ERR_CRYPTO_HASH_FINALIZED Digest already called
ERR_OSSL_EVP_NOT_XOF_OR_INVALID_LENGTH error:030000B2:digital envelope routines::not XOF or invalid length
0fdb27960308c51467edd49a0f5e0c434c9cca721f4c35bff005feabaf6010e777a1137ee8187c5288af57578d18d502a0bbe4c022f5587541961e10132d9834 e472c5e394f30ff8d5d33803f9593e63 qZk+NkcGgWq6PiVxeFDCbJzQ2J0=
stream 3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532
"###;

