//! Opt-in tests against real npm packages installed in `PI_JSRT_NPM_DIR` (skipped when unset):
//!
//! ```sh
//! npm install --ignore-scripts @anthropic-ai/sdk openai execa glob node-fetch ssh2 undici jose \
//!   node-forge tar archiver adm-zip got axios
//! PI_JSRT_NPM_DIR=$PWD cargo test -p pi-jsrt --test npm_packages
//! ```
//!
//! (`tests/modules.rs::real_npm_packages` covers ms, chalk, typebox, yaml, zod, diff, minimatch,
//! uuid, lodash-es, nanoid and strip-ansi.) The SDKs talk to a local mock server; network
//! packages talk to local servers. `--ignore-scripts` keeps ssh2 on its pure-JS crypto path (no
//! native addon), like pi-jsrt where addons cannot load.

use std::path::{Path, PathBuf};

use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn npm_dir() -> Option<PathBuf> {
    match std::env::var("PI_JSRT_NPM_DIR") {
        Ok(dir) => Some(dir.into()),
        Err(_) => {
            eprintln!("skipping: PI_JSRT_NPM_DIR not set");
            None
        }
    }
}

/// A mock server answering every request with `(content-type, body)` chosen by request path.
async fn mock_server(route: fn(&str) -> (&'static str, String)) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let head = loop {
                    let Ok(n) = socket.read(&mut chunk).await else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + length {
                            break text;
                        }
                    }
                };
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (content_type, body) = route(&path);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    base
}

fn sse(events: &[Value], named: bool) -> String {
    events
        .iter()
        .map(|e| match named {
            true => format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()),
            false => format!("data: {e}\n\n"),
        })
        .collect::<String>()
}

fn route(path: &str) -> (&'static str, String) {
    if path.starts_with("/v1/messages") {
        let events = [
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"mock","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello "}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"anthropic"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ];
        ("text/event-stream", sse(&events, true))
    } else if path.starts_with("/v1/chat/completions") {
        let chunk = |content: Value, finish: Value| {
            json!({"id":"c1","object":"chat.completion.chunk","created":1,"model":"mock","choices":[{"index":0,"delta":{"content":content},"finish_reason":finish}]})
        };
        let mut body = sse(&[chunk(json!("hello "), Value::Null), chunk(json!("openai"), Value::Null), chunk(Value::Null, json!("stop"))], false);
        body.push_str("data: [DONE]\n\n");
        ("text/event-stream", body)
    } else {
        ("application/json", json!({ "path": path }).to_string())
    }
}

async fn run(dir: &Path, source: &str) -> Value {
    let rt = JsRuntime::new(RuntimeOptions {
        cwd: Some(dir.to_path_buf()),
        ..Default::default()
    })
    .unwrap();
    let file = dir.join(format!("pi-jsrt-npm-test-{}.mjs", uuid::Uuid::new_v4()));
    std::fs::write(&file, source).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(120), rt.import(file.display().to_string()))
        .await
        .expect("timed out");
    let _ = std::fs::remove_file(&file);
    let ns = result.unwrap_or_else(|e| panic!("{e}\n{:?}", e.as_js().and_then(|j| j.stack.clone())));
    ns.get("default").await.unwrap()
}

/// `@anthropic-ai/sdk` (streaming with `messages.stream` and `create({ stream: true })`) and
/// `openai` (streamed chat completions). Exercises computed class method keys (which broke the
/// previous engine's scope analysis) and `FinalizationRegistry` cleanup jobs not blocking the job
/// queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_sdks_stream_from_a_mock_server() {
    let Some(dir) = npm_dir() else { return };
    let base = mock_server(route).await;
    let v = run(
        &dir,
        &format!(
            r#"
import Anthropic from "@anthropic-ai/sdk";
import OpenAI from "openai";
const anthropic = new Anthropic({{ apiKey: "sk-test", baseURL: "{base}" }});
const stream = anthropic.messages.stream({{ model: "m", max_tokens: 10, messages: [{{ role: "user", content: "hi" }}] }});
const deltas = [];
stream.on("text", (t) => deltas.push(t));
const final = await stream.finalMessage();
let raw = "";
for await (const event of await anthropic.messages.create({{ model: "m", max_tokens: 10, stream: true, messages: [{{ role: "user", content: "hi" }}] }})) {{
  if (event.type === "content_block_delta") raw += event.delta.text;
}}
const openai = new OpenAI({{ apiKey: "sk-test", baseURL: "{base}/v1" }});
let oai = "";
for await (const chunk of await openai.chat.completions.create({{ model: "m", stream: true, messages: [{{ role: "user", content: "hi" }}] }})) {{
  oai += chunk.choices[0]?.delta?.content ?? "";
}}
export default {{ deltas, text: final.content[0].text, stop: final.stop_reason, raw, oai }};
"#
        ),
    )
    .await;
    assert_eq!(
        v,
        json!({ "deltas": ["hello ", "anthropic"], "text": "hello anthropic", "stop": "end_turn", "raw": "hello anthropic", "oai": "hello openai" })
    );
}

/// execa (child processes through Node streams), glob (fs walking), node-fetch (ES module with a
/// trailing `export { ... }` list, which the previous engine's parser rejected).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_tooling_packages() {
    let Some(dir) = npm_dir() else { return };
    let v = run(
        &dir,
        r#"
import { execa } from "execa";
import { glob } from "glob";
import fetch, { Headers as NfHeaders } from "node-fetch";
const echo = await execa("echo", ["hi"]);
const piped = await execa("cat", { input: "from stdin" });
const failed = await execa("sh", ["-c", "exit 3"], { reject: false });
const files = (await glob("node_modules/execa/package.json")).sort();
export default {
  echo: echo.stdout, piped: piped.stdout, failed: failed.exitCode,
  files, fetch: typeof fetch, headers: new NfHeaders({ a: "1" }).get("a"),
};
"#,
    )
    .await;
    assert_eq!(
        v,
        json!({ "echo": "hi", "piped": "from stdin", "failed": 3, "files": ["node_modules/execa/package.json"], "fetch": "function", "headers": "1" })
    );
}

/// zlib consumers: tar (gzip streams through minipass), adm-zip (deflateRaw/inflateRaw sync) and
/// archiver (zip streams via readable-stream), cross-checked by reading archiver's zip with
/// adm-zip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zlib_packages_tar_adm_zip_archiver() {
    let Some(dir) = npm_dir() else { return };
    let work = tempfile::tempdir().unwrap();
    let v = run(
        &dir,
        &format!(
            r#"
import * as tar from "tar";
import AdmZip from "adm-zip";
import {{ ZipArchive }} from "archiver";
import fs from "node:fs";
import path from "node:path";
const work = {work:?};
fs.mkdirSync(path.join(work, "src/sub"), {{ recursive: true }});
fs.writeFileSync(path.join(work, "src/a.txt"), "alpha ".repeat(1000));
fs.writeFileSync(path.join(work, "src/sub/b.txt"), "beta");
await tar.c({{ gzip: true, file: path.join(work, "out.tgz"), cwd: work }}, ["src"]);
fs.mkdirSync(path.join(work, "x"));
await tar.x({{ file: path.join(work, "out.tgz"), cwd: path.join(work, "x") }});
const listed = [];
await tar.t({{ file: path.join(work, "out.tgz"), onReadEntry: (e) => listed.push(e.path) }});

const zip = new AdmZip();
zip.addFile("hello.txt", Buffer.from("hello zip ".repeat(100)));
zip.addLocalFolder(path.join(work, "src"), "src");
const zbuf = zip.toBuffer();
const back = new AdmZip(zbuf);

const output = fs.createWriteStream(path.join(work, "arch.zip"));
const archive = new ZipArchive({{ zlib: {{ level: 9 }} }});
const closed = new Promise((resolve, reject) => {{ output.on("close", resolve); archive.on("error", reject); }});
archive.pipe(output);
archive.append("from archiver ".repeat(50), {{ name: "note.txt" }});
archive.directory(path.join(work, "src"), "tree");
await archive.finalize();
await closed;
const arch = new AdmZip(path.join(work, "arch.zip"));
export default {{
  extracted: fs.readFileSync(path.join(work, "x/src/a.txt"), "utf8").length,
  b: fs.readFileSync(path.join(work, "x/src/sub/b.txt"), "utf8"),
  listed: listed.sort(),
  zipEntries: back.getEntries().map((e) => e.entryName).sort(),
  zipHello: back.readAsText("hello.txt").length,
  archEntries: arch.getEntries().map((e) => e.entryName).sort(),
  archNote: arch.readAsText("note.txt").length,
  archB: arch.readAsText("tree/sub/b.txt"),
}};
"#,
            work = work.path().display().to_string()
        ),
    )
    .await;
    assert_eq!(
        v,
        json!({
            "extracted": 6000, "b": "beta",
            "listed": ["src/", "src/a.txt", "src/sub/", "src/sub/b.txt"],
            "zipEntries": ["hello.txt", "src/a.txt", "src/sub/", "src/sub/b.txt"],
            "zipHello": 1000,
            "archEntries": ["note.txt", "tree/a.txt", "tree/sub/", "tree/sub/b.txt"],
            "archNote": 700, "archB": "beta",
        })
    );
}

/// jose (WebCrypto only since v6): JWS with HMAC/RSA/RSA-PSS/ECDSA/EdDSA keys (generate, JWK,
/// SPKI and PKCS#8 round trips), JWE with dir, ECDH-ES+A256KW, RSA-OAEP-256 and PBES2.
/// Output compared with Node 22.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jose_jwt_sign_verify_encrypt() {
    let Some(dir) = npm_dir() else { return };
    let v = run(&dir, r#"
import * as jose from "jose";
const out = {};
const secret = new TextEncoder().encode("a-string-secret-at-least-256-bits-long");
const hs = await new jose.SignJWT({ sub: "user", admin: true }).setProtectedHeader({ alg: "HS256" }).setIssuedAt(1700000000).setIssuer("pi").sign(secret);
out.hs = hs;
const { payload } = await jose.jwtVerify(hs, secret, { issuer: "pi" });
out.hsPayload = payload;
for (const alg of ["RS256", "PS256", "ES256", "ES384", "ES512", "EdDSA"]) {
  const { publicKey, privateKey } = await jose.generateKeyPair(alg, { extractable: true });
  const jwt = await new jose.SignJWT({ alg }).setProtectedHeader({ alg }).sign(privateKey);
  const v = await jose.jwtVerify(jwt, publicKey);
  const jwk = await jose.exportJWK(publicKey);
  const imported = await jose.importJWK(jwk, alg);
  const v2 = await jose.jwtVerify(jwt, imported);
  const spki = await jose.exportSPKI(publicKey);
  const fromSpki = await jose.importSPKI(spki, alg);
  await jose.jwtVerify(jwt, fromSpki);
  const pkcs8 = await jose.exportPKCS8(privateKey);
  await jose.importPKCS8(pkcs8, alg);
  out[alg] = v.payload.alg === alg && v2.payload.alg === alg;
}
const encKey = await jose.generateSecret("A256GCM");
const jwe = await new jose.CompactEncrypt(new TextEncoder().encode("secret text")).setProtectedHeader({ alg: "dir", enc: "A256GCM" }).encrypt(encKey);
out.jwe = new TextDecoder().decode((await jose.compactDecrypt(jwe, encKey)).plaintext);
const ecdhKey = await jose.generateKeyPair("ECDH-ES+A256KW", { extractable: true });
const jwe2 = await new jose.CompactEncrypt(new TextEncoder().encode("ecdh")).setProtectedHeader({ alg: "ECDH-ES+A256KW", enc: "A128CBC-HS256" }).encrypt(ecdhKey.publicKey);
out.jweEcdh = new TextDecoder().decode((await jose.compactDecrypt(jwe2, ecdhKey.privateKey)).plaintext);
const rsaOaep = await jose.generateKeyPair("RSA-OAEP-256", { extractable: true });
const jwe3 = await new jose.CompactEncrypt(new TextEncoder().encode("oaep")).setProtectedHeader({ alg: "RSA-OAEP-256", enc: "A256GCM" }).encrypt(rsaOaep.publicKey);
out.jweOaep = new TextDecoder().decode((await jose.compactDecrypt(jwe3, rsaOaep.privateKey)).plaintext);
const pbes = await new jose.CompactEncrypt(new TextEncoder().encode("pbes2")).setProtectedHeader({ alg: "PBES2-HS256+A128KW", enc: "A128GCM" }).encrypt(new TextEncoder().encode("password"));
out.pbes2 = new TextDecoder().decode((await jose.compactDecrypt(pbes, new TextEncoder().encode("password"), { keyManagementAlgorithms: ["PBES2-HS256+A128KW"] })).plaintext);
try { await jose.jwtVerify(hs, new TextEncoder().encode("wrong-secret-wrong-secret-wrong-secret")); } catch (e) { out.bad = e.code; }
export default out;
"#).await;
    assert_eq!(
        v,
        json!({
            "hs": "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1c2VyIiwiYWRtaW4iOnRydWUsImlhdCI6MTcwMDAwMDAwMCwiaXNzIjoicGkifQ.zubOoTfVwrYjJ1srezMuy-igxDH7p19Xt7v3m--28m8",
            "hsPayload": {"sub": "user", "admin": true, "iat": 1700000000, "iss": "pi"},
            "RS256": true, "PS256": true, "ES256": true, "ES384": true, "ES512": true, "EdDSA": true,
            "jwe": "secret text", "jweEcdh": "ecdh", "jweOaep": "oaep", "pbes2": "pbes2",
            "bad": "ERR_JWS_SIGNATURE_VERIFICATION_FAILED",
        })
    );
}

/// ssh2: an ssh2 `Server` and `Client` in the same runtime, over node:net. Covers curve25519,
/// ECDH P-256/P-521 and DH group14 key exchange, AES-GCM/CTR/CBC and chacha20-poly1305 (its
/// Poly1305 runs as WebAssembly), HMAC, zlib compression through Node's internal zlib handle,
/// Ed25519/RSA/ECDSA host keys and password auth. Output compared with Node 22.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh2_client_and_server() {
    let Some(dir) = npm_dir() else { return };
    let v = run(&dir, r#"
import ssh2 from "ssh2";
import crypto from "node:crypto";
const { Server, Client, utils } = ssh2;
const results = {};
async function session(hostKey, algorithms, label) {
  const server = new Server({ hostKeys: [hostKey], algorithms }, (client) => {
    client.on("authentication", (ctx) => {
      if (ctx.method === "password" && ctx.username === "pi" && ctx.password === "secret") ctx.accept();
      else if (ctx.method === "publickey" && ctx.username === "pi") ctx.accept();
      else ctx.reject(["password", "publickey"]);
    });
    client.on("ready", () => {
      client.on("session", (accept) => {
        const s = accept();
        s.once("exec", (accept, reject, info) => {
          const stream = accept();
          stream.write(`ran: ${info.command}`);
          stream.exit(0);
          stream.end();
        });
      });
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  const out = await new Promise((resolve, reject) => {
    const conn = new Client();
    let negotiated;
    conn.on("handshake", (n) => { negotiated = n; });
    conn.on("ready", () => {
      conn.exec("uptime", (err, stream) => {
        if (err) return reject(err);
        let data = "";
        stream.on("data", (d) => { data += d; });
        stream.on("close", (code) => { conn.end(); resolve({ data, code, kex: negotiated.kex, cipher: negotiated.cs.cipher, mac: negotiated.cs.mac, hostKey: negotiated.serverHostKey, compress: negotiated.cs.compress }); });
      });
    });
    conn.on("error", reject);
    conn.connect({ host: "127.0.0.1", port, username: "pi", password: "secret", algorithms });
  });
  await new Promise((r) => server.close(r));
  results[label] = out;
}
const ed = utils.generateKeyPairSync("ed25519").private;
const rsa = utils.generateKeyPairSync("rsa", { bits: 2048 }).private;
const ecdsa = utils.generateKeyPairSync("ecdsa", { bits: 256 }).private;
await session(ed, { kex: ["curve25519-sha256"], cipher: ["aes128-gcm@openssh.com"] }, "ed25519-gcm");
await session(rsa, { kex: ["ecdh-sha2-nistp256"], cipher: ["aes256-ctr"], hmac: ["hmac-sha2-256"], serverHostKey: ["rsa-sha2-256"] }, "rsa-ctr");
await session(ecdsa, { kex: ["diffie-hellman-group14-sha256"], cipher: ["aes128-cbc"], hmac: ["hmac-sha1"], compress: ["zlib@openssh.com", "zlib"] }, "ecdsa-dh-zlib");
await session(ed, { kex: ["ecdh-sha2-nistp521"], cipher: ["chacha20-poly1305@openssh.com"] }, "chacha");
results.parsed = utils.parseKey(rsa).type;
export default results;
"#).await;
    let session = |kex: &str, cipher: &str, mac: &str, host_key: &str, compress: &str| {
        json!({"data": "ran: uptime", "code": 0, "kex": kex, "cipher": cipher, "mac": mac, "hostKey": host_key, "compress": compress})
    };
    assert_eq!(
        v,
        json!({
            "ed25519-gcm": session("curve25519-sha256", "aes128-gcm@openssh.com", "", "ssh-ed25519", "none"),
            "rsa-ctr": session("ecdh-sha2-nistp256", "aes256-ctr", "hmac-sha2-256", "rsa-sha2-256", "none"),
            "ecdsa-dh-zlib": session("diffie-hellman-group14-sha256", "aes128-cbc", "hmac-sha1", "ecdsa-sha2-nistp256", "zlib@openssh.com"),
            "chacha": session("ecdh-sha2-nistp521", "chacha20-poly1305@openssh.com", "", "ssh-ed25519", "none"),
            "parsed": "ssh-rsa",
        })
    );
}

/// undici: `request`, `fetch` and a `Pool` against a local node:http server. undici parses
/// HTTP with llhttp compiled to WebAssembly. Output compared with Node 22.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undici_request_fetch_pool() {
    let Some(dir) = npm_dir() else { return };
    let v = run(&dir, r#"
import { request, fetch, Agent, Pool } from "undici";
import http from "node:http";
const server = http.createServer((req, res) => {
  let body = "";
  req.on("data", (d) => (body += d));
  req.on("end", () => {
    if (req.url === "/chunked") {
      res.writeHead(200, { "content-type": "text/plain" });
      res.write("part1-");
      setTimeout(() => res.end("part2"), 10);
      return;
    }
    res.writeHead(201, { "content-type": "application/json", "x-test": "yes" });
    res.end(JSON.stringify({ method: req.method, url: req.url, body, ua: req.headers["user-agent"] ?? null }));
  });
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${server.address().port}`;
const out = {};
const r1 = await request(`${base}/req?x=1`, { method: "POST", body: "hello", headers: { "content-type": "text/plain" } });
out.request = { status: r1.statusCode, header: r1.headers["x-test"], body: await r1.body.json() };
const r2 = await fetch(`${base}/fetch`, { method: "PUT", body: JSON.stringify({ a: 1 }) });
out.fetch = { status: r2.status, ok: r2.ok, body: await r2.json() };
const r3 = await request(`${base}/chunked`);
out.chunked = await r3.body.text();
const pool = new Pool(base, { connections: 2 });
const many = await Promise.all([1, 2, 3, 4].map(async (i) => (await (await pool.request({ path: `/p${i}`, method: "GET" })).body.json()).url));
out.pool = many;
await pool.close();
server.close();
export default out;
"#).await;
    assert_eq!(
        v,
        json!({
            "request": {"status": 201, "header": "yes", "body": {"method": "POST", "url": "/req?x=1", "body": "hello", "ua": null}},
            "fetch": {"status": 201, "ok": true, "body": {"method": "PUT", "url": "/fetch", "body": "{\"a\":1}", "ua": "undici"}},
            "chunked": "part1-part2",
            "pool": ["/p1", "/p2", "/p3", "/p4"],
        })
    );
}

/// got, axios and node-forge against a local `https.createServer` with a certificate from a test
/// CA: trusted via `https.certificateAuthority` / an `https.Agent` with `ca`, redirects,
/// verification failures for untrusted requests. Output compared with Node 22.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn got_axios_forge_over_https() {
    let Some(dir) = npm_dir() else { return };
    let v = run(&dir, r#"
import https from "node:https";
import got from "got";
import axios from "axios";
import forge from "node-forge";
const certs = {"ca": "-----BEGIN CERTIFICATE-----\nMIIBijCCATGgAwIBAgIUVs+EfMS/+Ro7R0lcA4uc/AhPkfcwCgYIKoZIzj0EAwIw\nGjEYMBYGA1UEAwwPcGktanNydCB0ZXN0IENBMCAXDTI2MTAyMjE0NTQ1NloYDzIx\nMjYwOTI4MTQ1NDU2WjAaMRgwFgYDVQQDDA9waS1qc3J0IHRlc3QgQ0EwWTATBgcq\nhkjOPQIBBggqhkjOPQMBBwNCAAQXCcWuLg51sJCkC2Jjdx3JnX9g30cJm2/Q+ntm\nIKTAK1Qf33OATy1sjP94DopCai3wP+eOi5/On5xoGRfrwSuLo1MwUTAdBgNVHQ4E\nFgQUT/pxUc0B07VmR1bUJ1CjH3qtNwYwHwYDVR0jBBgwFoAUT/pxUc0B07VmR1bU\nJ1CjH3qtNwYwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNHADBEAiBAJYPI\nkFx24T48mhhbpia1ZkznObsjhiqwHClxPMIeDgIgJThIRiRm0g2PuaoNQFBJzvKx\nnT4Y2EGoPBoqvDax6+k=\n-----END CERTIFICATE-----\n", "key": "-----BEGIN EC PRIVATE KEY-----\nMHcCAQEEIDMUcB3eZ4nh1AzrsEMvsmqcOxfnBpjfLWJUJWivV79QoAoGCCqGSM49\nAwEHoUQDQgAECsujzoaPh8daHDbzr+W8cUzLjRqqoXS57N2//PYAZ7IMr47Pkxto\nbCvCjp2YpgHhoxfaNUQSnnpnt88TTmKQew==\n-----END EC PRIVATE KEY-----\n", "cert": "-----BEGIN CERTIFICATE-----\nMIIByjCCAW+gAwIBAgIUY2YuzVK4HDbIzlyNdAw/u74U/7UwCgYIKoZIzj0EAwIw\nGjEYMBYGA1UEAwwPcGktanNydCB0ZXN0IENBMCAXDTI2MTAyMjE0NTQ1NloYDzIx\nMjYwOTI4MTQ1NDU2WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIB\nBggqhkjOPQMBBwNCAAQKy6POho+Hx1ocNvOv5bxxTMuNGqqhdLns3b/89gBnsgyv\njs+TG2hsK8KOnZimAeGjF9o1RBKeeme3zxNOYpB7o4GWMIGTMBoGA1UdEQQTMBGC\nCWxvY2FsaG9zdIcEfwAAATAJBgNVHRMEAjAAMAsGA1UdDwQEAwIHgDAdBgNVHSUE\nFjAUBggrBgEFBQcDAQYIKwYBBQUHAwIwHQYDVR0OBBYEFEnI8FK6Ovo9sspTt0r9\nUzpPhZDQMB8GA1UdIwQYMBaAFE/6cVHNAdO1ZkdW1CdQox96rTcGMAoGCCqGSM49\nBAMCA0kAMEYCIQCSxhvJ5bcdbziBydqRjR7cFGlcu7mBHseV5NDOkRkd7QIhAK0K\nHb6U4U6rbzNvIl+I/L+wKz97yKaglZ5BgB0aHSxB\n-----END CERTIFICATE-----\n"};
const server = https.createServer({ key: certs.key, cert: certs.cert }, (req, res) => {
  let body = "";
  req.on("data", (d) => (body += d));
  req.on("end", () => {
    if (req.url === "/redirect") { res.writeHead(302, { location: "/json" }); res.end(); return; }
    res.setHeader("content-type", "application/json");
    res.end(JSON.stringify({ method: req.method, url: req.url, body, encrypted: req.socket.encrypted === true }));
  });
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const base = `https://localhost:${server.address().port}`;
const out = {};
out.got = await got.post(`${base}/got`, { json: { a: 1 }, https: { certificateAuthority: certs.ca } }).json();
out.gotRedirect = (await got(`${base}/redirect`, { https: { certificateAuthority: certs.ca } }).json()).url;
try { await got(`${base}/x`, { retry: { limit: 0 } }); } catch (e) { out.gotUntrusted = e.code; }
const agent = new https.Agent({ ca: certs.ca });
const ax = await axios.put(`${base}/axios`, "payload", { httpsAgent: agent, headers: { "content-type": "text/plain" } });
out.axios = { status: ax.status, data: ax.data };
try { await axios.get(`${base}/x`); } catch (e) { out.axiosUntrusted = e.code; }
// node-forge: pure-JS crypto (its random source is node:crypto).
const key = forge.pkcs5.pbkdf2("password", "salt", 1000, 16);
const cipher = forge.cipher.createCipher("AES-CBC", key);
const iv = forge.random.getBytesSync(16);
cipher.start({ iv });
cipher.update(forge.util.createBuffer("forge secret"));
cipher.finish();
const decipher = forge.cipher.createDecipher("AES-CBC", key);
decipher.start({ iv });
decipher.update(cipher.output);
decipher.finish();
out.forge = { roundTrip: decipher.output.toString(), sha256: forge.md.sha256.create().update("abc").digest().toHex().slice(0, 16), keyHex: forge.util.bytesToHex(key) };
server.close();
export default out;
"#).await;
    let expected: Value = serde_json::from_str(r#"{"got":{"method":"POST","url":"/got","body":"{\"a\":1}","encrypted":true},"gotRedirect":"/json","gotUntrusted":"UNABLE_TO_VERIFY_LEAF_SIGNATURE","axios":{"status":200,"data":{"method":"PUT","url":"/axios","body":"payload","encrypted":true}},"axiosUntrusted":"UNABLE_TO_VERIFY_LEAF_SIGNATURE","forge":{"roundTrip":"forge secret","sha256":"ba7816bf8f01cfea","keyHex":"6e88be8bad7eae9d9e10aa061224034f"}}"#).unwrap();
    assert_eq!(v, expected);
}
