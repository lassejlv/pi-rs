//! `node:http2` (h2 crate, sans I/O over net/tls sockets): core and compatibility APIs,
//! TLS with ALPN and the HTTP/1.1 fallback, trailers, resets, ping, flow control. Expected
//! outputs are Node 22's; with `node` on PATH, Node and pi-jsrt are also run against each other.

use std::process::Command;

const NODE_BIN: &str = env!("CARGO_BIN_EXE_pi-jsrt-node");

fn run(dir: &std::path::Path, name: &str, script: &str, args: &[&str]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, script).unwrap();
    let out = Command::new(NODE_BIN).arg(&path).args(args).output().unwrap();
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

fn without_ports(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("localhost:") {
        out.push_str(&rest[..i + 10]);
        rest = rest[i + 10..].trim_start_matches(|c: char| c.is_ascii_digit());
        out.push_str("PORT");
    }
    out.push_str(rest);
    out
}

#[test]
fn http2_core_api_trailers_and_resets() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run(dir.path(), "h2a.js", H2A, &[]), H2A_NODE);
}

#[test]
fn http2_secure_compat_api_ping_and_http1_fallback() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("certs.json"), CERTS).unwrap();
    assert_eq!(without_ports(&run(dir.path(), "h2b.js", H2B, &[])), H2B_NODE);
}

#[test]
fn http2_interoperates_with_node() {
    let Ok(node) = Command::new("node").arg("--version").output() else { return };
    if !node.status.success() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("srv.js"), SRV).unwrap();
    std::fs::write(dir.path().join("cli.js"), CLI).unwrap();
    for (server, client) in [("node", NODE_BIN), (NODE_BIN, "node")] {
        let mut srv = Command::new(server).arg(dir.path().join("srv.js")).stdout(std::process::Stdio::piped()).spawn().unwrap();
        let mut port = String::new();
        {
            use std::io::BufRead;
            std::io::BufReader::new(srv.stdout.as_mut().unwrap()).read_line(&mut port).unwrap();
        }
        let out = Command::new(client).arg(dir.path().join("cli.js")).arg(port.trim()).output().unwrap();
        let _ = srv.kill();
        let _ = srv.wait();
        assert_eq!(String::from_utf8_lossy(&out.stdout), CROSS, "server {server}, client {client}");
    }
}

const CROSS: &str = r#"[["/a",200,"srv"],["/a",7,"GET /a "],["/b",100007,"PUT /b xxxxx"],["/b",200,"srv"],["/c",200,"srv"],["/c",8,"PUT /c z"]]
"#;

const H2A: &str = r###"const http2 = require("http2");
const log = [];
const server = http2.createServer();
server.on("stream", (stream, headers, flags) => {
  log.push(["server stream", headers[":method"], headers[":path"], headers[":scheme"], headers["x-custom"], typeof flags, stream.id]);
  let body = "";
  stream.setEncoding("utf8");
  stream.on("data", (d) => (body += d));
  stream.on("end", () => {
    log.push(["server body", body]);
    if (headers[":path"] === "/reset") { stream.on("error", (e) => log.push(["server stream error", e.code])); stream.close(http2.constants.NGHTTP2_INTERNAL_ERROR); return; }
    stream.respond({ ":status": 200, "content-type": "text/plain", "set-cookie": ["a=1", "b=2"], "x-multi": ["p", "q"] }, { waitForTrailers: true });
    stream.on("wantTrailers", () => stream.sendTrailers({ "x-trailer": "done" }));
    stream.write("hello ");
    stream.end("world " + body.length);
  });
});
server.listen(0, "127.0.0.1", () => {
  const port = server.address().port;
  const client = http2.connect(`http://127.0.0.1:${port}`);
  client.on("connect", (session, socket) => log.push(["connect", session === client, client.alpnProtocol, client.encrypted, typeof socket]));
  const req = client.request({ ":path": "/upload", ":method": "POST", "x-custom": "yes" });
  req.on("response", (headers, flags) => log.push(["response", headers[":status"], headers["content-type"], headers["set-cookie"], headers["x-multi"], typeof headers.date, flags]));
  req.on("trailers", (t) => log.push(["trailers", t["x-trailer"]]));
  req.setEncoding("utf8");
  let data = "";
  req.on("data", (d) => (data += d));
  req.on("end", () => {
    log.push(["client body", data]);
    const r2 = client.request({ ":path": "/reset" });
    r2.end();
    r2.on("error", (e) => log.push(["r2 error", e.code, e.message]));
    r2.on("close", () => {
      log.push(["r2 close", r2.rstCode]);
      client.close(() => { log.push(["client closed"]); server.close(() => { log.push(["server closed"]); console.log(JSON.stringify(log, null, 0)); }); });
    });
  });
  req.write("ab");
  req.end("c");
});
"###;
const H2A_NODE: &str = r###"[["connect",true,"h2c",false,"object"],["server stream","POST","/upload","http","yes","number",1],["server body","abc"],["response",200,"text/plain",["a=1","b=2"],"p, q","string",4],["trailers","done"],["client body","hello world 3"],["server stream","GET","/reset","http",null,"number",3],["server body",""],["server stream error","ERR_HTTP2_STREAM_ERROR"],["r2 error","ERR_HTTP2_STREAM_ERROR","Stream closed with error code NGHTTP2_INTERNAL_ERROR"],["r2 close",2],["client closed"],["server closed"]]
"###;
const H2B: &str = r###"// Secure server with the compatibility API, allowHTTP1, ping; client with ca.
const http2 = require("http2");
const https = require("https");
const certs = JSON.parse(require("fs").readFileSync(__dirname + "/certs.json", "utf8"));
const log = [];
const server = http2.createSecureServer({ key: certs.leafKey, cert: certs.leafCert, allowHTTP1: true }, (req, res) => {
  let body = "";
  req.setEncoding("utf8");
  req.on("data", (d) => (body += d));
  req.on("end", () => {
    log.push(["req", req.httpVersion, req.method, req.url, req.headers["x-a"], body, req.authority ?? null, req.scheme ?? null]);
    res.setHeader("X-Reply", "1");
    res.statusCode = req.url === "/missing" ? 404 : 201;
    res.addTrailers?.({ "x-t": "v" });
    res.end(`got:${body}`);
  });
});
server.listen(0, "127.0.0.1", async () => {
  const port = server.address().port;
  const client = http2.connect(`https://localhost:${port}`, { ca: certs.ca });
  await new Promise((r) => client.once("connect", r));
  log.push(["alpn", client.alpnProtocol, client.encrypted]);
  const fetchOne = (path, body) => new Promise((resolve) => {
    const req = client.request({ ":path": path, ":method": body ? "POST" : "GET", "x-a": "b" });
    let data = "";
    let status;
    req.on("response", (h) => (status = [h[":status"], h["x-reply"]]));
    req.on("trailers", (t) => log.push(["trailers", t["x-t"]]));
    req.on("data", (d) => (data += d));
    req.on("end", () => resolve([...status, data]));
    if (body) req.end(body);
  });
  log.push(["r1", await fetchOne("/one", "payload")]);
  log.push(["r2", await fetchOne("/missing")]);
  const ping = await new Promise((resolve) => client.ping(Buffer.from("abcdefgh"), (err, ms, payload) => resolve([err, typeof ms, payload.length])));
  log.push(["ping", ping]);
  // HTTP/1.1 fallback on the same server.
  const h1 = await new Promise((resolve) => https.get(`https://localhost:${port}/h1`, { ca: certs.ca }, (res) => { let d = ""; res.on("data", (c) => d += c); res.on("end", () => resolve([res.statusCode, res.httpVersion, d])); }));
  log.push(["h1", h1]);
  client.close();
  server.close(() => console.log(JSON.stringify(log)));
});
"###;
const H2B_NODE: &str = r###"[["alpn","h2",true],["req","2.0","POST","/one","b","payload","localhost:PORT","https"],["trailers","v"],["r1",[201,"1","got:payload"]],["req","2.0","GET","/missing","b","","localhost:PORT","https"],["trailers","v"],["r2",[404,"1","got:"]],["ping",[null,"number",8]],["req","1.1","GET","/h1",null,"",null,null],["h1",[201,"1.1","got:"]]]
"###;
const CERTS: &str = r###"{"ca": "-----BEGIN CERTIFICATE-----\nMIIBijCCATGgAwIBAgIUVs+EfMS/+Ro7R0lcA4uc/AhPkfcwCgYIKoZIzj0EAwIw\nGjEYMBYGA1UEAwwPcGktanNydCB0ZXN0IENBMCAXDTI2MTAyMjE0NTQ1NloYDzIx\nMjYwOTI4MTQ1NDU2WjAaMRgwFgYDVQQDDA9waS1qc3J0IHRlc3QgQ0EwWTATBgcq\nhkjOPQIBBggqhkjOPQMBBwNCAAQXCcWuLg51sJCkC2Jjdx3JnX9g30cJm2/Q+ntm\nIKTAK1Qf33OATy1sjP94DopCai3wP+eOi5/On5xoGRfrwSuLo1MwUTAdBgNVHQ4E\nFgQUT/pxUc0B07VmR1bUJ1CjH3qtNwYwHwYDVR0jBBgwFoAUT/pxUc0B07VmR1bU\nJ1CjH3qtNwYwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNHADBEAiBAJYPI\nkFx24T48mhhbpia1ZkznObsjhiqwHClxPMIeDgIgJThIRiRm0g2PuaoNQFBJzvKx\nnT4Y2EGoPBoqvDax6+k=\n-----END CERTIFICATE-----\n", "leafKey": "-----BEGIN EC PRIVATE KEY-----\nMHcCAQEEIDMUcB3eZ4nh1AzrsEMvsmqcOxfnBpjfLWJUJWivV79QoAoGCCqGSM49\nAwEHoUQDQgAECsujzoaPh8daHDbzr+W8cUzLjRqqoXS57N2//PYAZ7IMr47Pkxto\nbCvCjp2YpgHhoxfaNUQSnnpnt88TTmKQew==\n-----END EC PRIVATE KEY-----\n", "leafCert": "-----BEGIN CERTIFICATE-----\nMIIByjCCAW+gAwIBAgIUY2YuzVK4HDbIzlyNdAw/u74U/7UwCgYIKoZIzj0EAwIw\nGjEYMBYGA1UEAwwPcGktanNydCB0ZXN0IENBMCAXDTI2MTAyMjE0NTQ1NloYDzIx\nMjYwOTI4MTQ1NDU2WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIB\nBggqhkjOPQMBBwNCAAQKy6POho+Hx1ocNvOv5bxxTMuNGqqhdLns3b/89gBnsgyv\njs+TG2hsK8KOnZimAeGjF9o1RBKeeme3zxNOYpB7o4GWMIGTMBoGA1UdEQQTMBGC\nCWxvY2FsaG9zdIcEfwAAATAJBgNVHRMEAjAAMAsGA1UdDwQEAwIHgDAdBgNVHSUE\nFjAUBggrBgEFBQcDAQYIKwYBBQUHAwIwHQYDVR0OBBYEFEnI8FK6Ovo9sspTt0r9\nUzpPhZDQMB8GA1UdIwQYMBaAFE/6cVHNAdO1ZkdW1CdQox96rTcGMAoGCCqGSM49\nBAMCA0kAMEYCIQCSxhvJ5bcdbziBydqRjR7cFGlcu7mBHseV5NDOkRkd7QIhAK0K\nHb6U4U6rbzNvIl+I/L+wKz97yKaglZ5BgB0aHSxB\n-----END CERTIFICATE-----\n", "selfKey": "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgdgY8dymE4ogssKPY\nwiPUuUK/Vd7p24Pf66MkNRxPvkWhRANCAARyoIUirzoFMXRzf/h3glvrCpsBC834\n5x9X36pyLsrgn3025Y7liAVHHIsIW2I2G0esamjWwQLxTITOXoFe1pUW\n-----END PRIVATE KEY-----\n", "selfCert": "-----BEGIN CERTIFICATE-----\nMIIBmzCCAUGgAwIBAgIUS/1oasIwSWyNdibO8FDTpMXj8kkwCgYIKoZIzj0EAwIw\nFDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTAyMjE0NTQ1NloYDzIxMjYwOTI4\nMTQ1NDU2WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO\nPQMBBwNCAARyoIUirzoFMXRzf/h3glvrCpsBC8345x9X36pyLsrgn3025Y7liAVH\nHIsIW2I2G0esamjWwQLxTITOXoFe1pUWo28wbTAdBgNVHQ4EFgQUJ1PVpBqrN9iN\n3/LdV3jFPioUuDwwHwYDVR0jBBgwFoAUJ1PVpBqrN9iN3/LdV3jFPioUuDwwDwYD\nVR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCgYIKoZI\nzj0EAwIDSAAwRQIhAK2VRsHWv2v8SxSKEM/mi+KImGGhovgXStaw3D1xRujIAiBq\nJc1DQbnRlymy8y/B1HyIz5UYShVBzBNH2IX+Y4caOA==\n-----END CERTIFICATE-----\n"}
"###;
const SRV: &str = r###"const http2 = require("http2");
const server = http2.createServer((req, res) => { let b = ""; req.on("data", (d) => b += d); req.on("end", () => { res.writeHead(200, { "x-impl": "srv" }); res.end(`${req.method} ${req.url} ${b}`); }); });
server.listen(0, "127.0.0.1", () => { console.log(server.address().port); });
setTimeout(() => server.close(), 3000).unref?.();
process.stdin?.on?.("data", () => {});
"###;
const CLI: &str = r###"const http2 = require("http2");
const c = http2.connect(`http://127.0.0.1:${process.argv[2]}`);
const out = [];
let n = 0;
for (const [p, body] of [["/a", null], ["/b", "x".repeat(100000)], ["/c", "z"]]) {
  const req = c.request({ ":path": p, ":method": body ? "PUT" : "GET" });
  let d = ""; req.setEncoding("utf8");
  req.on("response", (h) => out.push([p, h[":status"], h["x-impl"]]));
  req.on("data", (x) => d += x);
  req.on("end", () => { out.push([p, d.length, d.slice(0, 12)]); if (++n === 3) { c.close(); console.log(JSON.stringify(out.sort())); } });
  if (body) req.end(body);
}
"###;
