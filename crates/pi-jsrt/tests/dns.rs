//! `node:dns` resolve*/reverse/lookupService through the stub resolver against a local DNS
//! server (itself a pi-jsrt dgram script). Expected output is Node 22's for the same client.

use std::io::BufRead;
use std::process::{Command, Stdio};

const NODE_BIN: &str = env!("CARGO_BIN_EXE_pi-jsrt-node");

#[test]
fn dns_resolver_matches_node() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("server.js"), SERVER).unwrap();
    std::fs::write(dir.path().join("client.js"), CLIENT).unwrap();
    let mut server = Command::new(NODE_BIN).arg(dir.path().join("server.js")).stdout(Stdio::piped()).spawn().unwrap();
    let mut port = String::new();
    std::io::BufReader::new(server.stdout.as_mut().unwrap()).read_line(&mut port).unwrap();
    let port = port.trim().to_string();
    let out = Command::new(NODE_BIN).arg(dir.path().join("client.js")).env("DNS_PORT", &port).output().unwrap();
    let _ = server.kill();
    let _ = server.wait();
    let text = String::from_utf8_lossy(&out.stdout).replace(&format!("127.0.0.1:{port}"), "127.0.0.1:PORT");
    assert_eq!(text, EXPECTED, "{}", String::from_utf8_lossy(&out.stderr));
}

const SERVER: &str = r###"// Minimal authoritative DNS server for tests (UDP), records below.
const dgram = require("dgram");
function name(n) { const out = []; for (const l of n.split(".").filter(Boolean)) { out.push(l.length, ...Buffer.from(l)); } out.push(0); return Buffer.from(out); }
function rr(owner, type, ttl, rdata) { const h = Buffer.alloc(10); h.writeUInt16BE(type, 0); h.writeUInt16BE(1, 2); h.writeUInt32BE(ttl, 4); h.writeUInt16BE(rdata.length, 8); return Buffer.concat([name(owner), h, rdata]); }
const u16 = (n) => { const b = Buffer.alloc(2); b.writeUInt16BE(n); return b; };
const u32 = (n) => { const b = Buffer.alloc(4); b.writeUInt32BE(n); return b; };
const str = (s) => Buffer.concat([Buffer.from([Buffer.byteLength(s)]), Buffer.from(s)]);
const records = {
  "a.test": [[1, Buffer.from([1, 2, 3, 4])], [1, Buffer.from([5, 6, 7, 8])], [28, Buffer.from("00000000000000000000000000000001", "hex")]],
  "alias.test": [[5, name("a.test")]],
  "mx.test": [[15, Buffer.concat([u16(10), name("mail.test")])], [15, Buffer.concat([u16(20), name("backup.test")])]],
  "txt.test": [[16, Buffer.concat([str("v=spf1"), str("x")])], [16, str("second")]],
  "srv.test": [[33, Buffer.concat([u16(1), u16(2), u16(8080), name("target.test")])]],
  "ns.test": [[2, name("ns1.test")], [2, name("ns2.test")]],
  "soa.test": [[6, Buffer.concat([name("ns1.test"), name("admin.test"), u32(2024010101), u32(3600), u32(600), u32(86400), u32(300)])]],
  "caa.test": [[257, Buffer.concat([Buffer.from([0]), str("issue"), Buffer.from("letsencrypt.org")])], [257, Buffer.concat([Buffer.from([128]), str("iodef"), Buffer.from("mailto:a@b.test")])]],
  "naptr.test": [[35, Buffer.concat([u16(100), u16(10), str("u"), str("E2U+sip"), str("!^.*$!sip:x@y.test!"), name("")])]],
  "4.3.2.1.in-addr.arpa": [[12, name("host.test")]],
  "nodata.test": [],
};
const sock = dgram.createSocket("udp4");
sock.on("message", (msg, rinfo) => {
  const id = msg.readUInt16BE(0);
  let off = 12; const labels = [];
  while (msg[off]) { labels.push(msg.subarray(off + 1, off + 1 + msg[off]).toString()); off += 1 + msg[off]; }
  off++;
  const qtype = msg.readUInt16BE(off);
  const q = labels.join(".").toLowerCase();
  const question = msg.subarray(12, off + 4);
  let rcode = 0; let answers = [];
  if (q === "servfail.test") rcode = 2;
  else if (q === "refused.test") rcode = 5;
  else if (!(q in records)) rcode = 3;
  else answers = records[q].filter(([t]) => qtype === 255 || t === qtype || (t === 5 && qtype !== 5)).map(([t, d]) => rr(q, t, 300, d));
  if (q === "timeout.test") return;
  const header = Buffer.alloc(12); header.writeUInt16BE(id, 0); header.writeUInt16BE(0x8180 | rcode, 2); header.writeUInt16BE(1, 4); header.writeUInt16BE(answers.length, 6);
  sock.send(Buffer.concat([header, question, ...answers]), rinfo.port, rinfo.address);
});
sock.bind(Number(process.argv[2] || 0), "127.0.0.1", () => console.log(sock.address().port));
"###;
const CLIENT: &str = r###"const dns = require("dns");
const server = `127.0.0.1:${process.env.DNS_PORT}`;
const r = new dns.promises.Resolver({ timeout: 300, tries: 1 });
r.setServers([server]);
const cr = new dns.Resolver();
cr.setServers([server]);
const out = [];
const sortTxt = (v) => v;
(async () => {
  const tryIt = async (label, fn) => { try { out.push([label, await fn()]); } catch (e) { out.push([label, "ERR", e.code, e.syscall, e.hostname, e.message, e.errno]); } };
  await tryIt("getServers", () => r.getServers());
  await tryIt("resolve4", () => r.resolve4("a.test"));
  await tryIt("resolve4 ttl", () => r.resolve4("a.test", { ttl: true }));
  await tryIt("resolve6", () => r.resolve6("a.test"));
  await tryIt("resolve6 ttl", () => r.resolve6("a.test", { ttl: true }));
  await tryIt("resolveCname", () => r.resolveCname("alias.test"));
  await tryIt("resolveMx", () => r.resolveMx("mx.test"));
  await tryIt("resolveTxt", () => r.resolveTxt("txt.test"));
  await tryIt("resolveSrv", () => r.resolveSrv("srv.test"));
  await tryIt("resolveNs", () => r.resolveNs("ns.test"));
  await tryIt("resolveSoa", () => r.resolveSoa("soa.test"));
  await tryIt("resolveCaa", () => r.resolveCaa("caa.test"));
  await tryIt("resolveNaptr", () => r.resolveNaptr("naptr.test"));
  await tryIt("resolvePtr", () => r.resolvePtr("4.3.2.1.in-addr.arpa"));
  await tryIt("resolve MX", () => r.resolve("mx.test", "MX"));
  await tryIt("resolve default", () => r.resolve("a.test"));
  await tryIt("reverse", () => r.reverse("1.2.3.4"));
  await tryIt("nxdomain", () => r.resolve4("missing.test"));
  await tryIt("nodata", () => r.resolveTxt("nodata.test"));
  await tryIt("servfail", () => r.resolveMx("servfail.test"));
  await tryIt("refused", () => r.resolve4("refused.test"));
  await tryIt("timeout", () => r.resolve4("timeout.test"));
  await tryIt("bad rrtype", () => r.resolve("a.test", "BOGUS"));
  await tryIt("callback", () => new Promise((res, rej) => cr.resolveMx("mx.test", (err, v) => (err ? rej(err) : res(v)))));
  await tryIt("callback err", () => new Promise((res, rej) => cr.resolve4("missing.test", (err, v) => (err ? rej(err) : res(v)))));
  await tryIt("lookupService", () => dns.promises.lookupService("127.0.0.1", 22));
  try { r.setServers(["not an ip"]); } catch (e) { out.push(["setServers", e.code, e.message]); }
  console.log(JSON.stringify(out));
})();
"###;
const EXPECTED: &str = r###"[["getServers",["127.0.0.1:PORT"]],["resolve4",["1.2.3.4","5.6.7.8"]],["resolve4 ttl",[{"address":"1.2.3.4","ttl":300},{"address":"5.6.7.8","ttl":300}]],["resolve6",["::1"]],["resolve6 ttl",[{"address":"::1","ttl":300}]],["resolveCname",["a.test"]],["resolveMx",[{"exchange":"mail.test","priority":10},{"exchange":"backup.test","priority":20}]],["resolveTxt",[["v=spf1","x"],["second"]]],["resolveSrv",[{"name":"target.test","port":8080,"priority":1,"weight":2}]],["resolveNs",["ns1.test","ns2.test"]],["resolveSoa",{"nsname":"ns1.test","hostmaster":"admin.test","serial":2024010101,"refresh":3600,"retry":600,"expire":86400,"minttl":300}],["resolveCaa",[{"critical":0,"issue":"letsencrypt.org"},{"critical":128,"iodef":"mailto:a@b.test"}]],["resolveNaptr",[{"flags":"u","service":"E2U+sip","regexp":"!^.*$!sip:x@y.test!","replacement":"","order":100,"preference":10}]],["resolvePtr",["host.test"]],["resolve MX",[{"exchange":"mail.test","priority":10},{"exchange":"backup.test","priority":20}]],["resolve default",["1.2.3.4","5.6.7.8"]],["reverse",["host.test"]],["nxdomain","ERR","ENOTFOUND","queryA","missing.test","queryA ENOTFOUND missing.test",null],["nodata","ERR","ENODATA","queryTxt","nodata.test","queryTxt ENODATA nodata.test",null],["servfail","ERR","ESERVFAIL","queryMx","servfail.test","queryMx ESERVFAIL servfail.test",null],["refused","ERR","EREFUSED","queryA","refused.test","queryA EREFUSED refused.test",null],["timeout","ERR","ETIMEOUT","queryA","timeout.test","queryA ETIMEOUT timeout.test",null],["bad rrtype","ERR","ERR_INVALID_ARG_VALUE",null,null,"The argument 'rrtype' is invalid. Received 'BOGUS'",null],["callback",[{"exchange":"mail.test","priority":10},{"exchange":"backup.test","priority":20}]],["callback err","ERR","ENOTFOUND","queryA","missing.test","queryA ENOTFOUND missing.test",null],["lookupService",{"hostname":"localhost","service":"ssh"}],["setServers","ERR_INVALID_IP_ADDRESS","Invalid IP address: not an ip"]]
"###;
