//! Node API shims: fs, path, os, child_process, events, util, buffer, crypto, fetch, streams.

use std::sync::Arc;

use parking_lot::Mutex;
use pi_jsrt::{JsRuntime, RuntimeOptions};
use serde_json::{Value, json};

fn rt_in(dir: &std::path::Path) -> JsRuntime {
    JsRuntime::new(RuntimeOptions {
        // Compare external pwd against the physical cwd, as Node does after chdir.
        cwd: Some(dir.canonicalize().unwrap()),
        ..Default::default()
    })
    .unwrap()
}

async fn eval_module(rt: &JsRuntime, src: &str) -> Value {
    let ns = rt.eval_module(src, "test-main.mjs").await.unwrap_or_else(|e| panic!("{e}\n{:?}", e.as_js().and_then(|j| j.stack.clone())));
    ns.get("default").await.unwrap()
}

#[tokio::test]
async fn path_and_os() {
    let dir = tempfile::tempdir().unwrap();
    let rt = rt_in(dir.path());
    let v = eval_module(
        &rt,
        r#"
        import path from "node:path";
        import { join, basename } from "path";
        import * as os from "node:os";
        export default {
            join: join("/a/b", "../c", "./d.txt"),
            rel: path.relative("/a/b/c", "/a/d"),
            resolve: path.resolve("x") === path.join(process.cwd(), "x"),
            parse: path.parse("/home/u/file.tar.gz"),
            base: basename("/x/y.ts", ".ts"),
            ext: path.extname("a/b.c/d.json"),
            norm: path.normalize("a//b/../c/"),
            dir: path.dirname("/a/b/c"),
            abs: path.isAbsolute("/x") && !path.isAbsolute("x"),
            eol: os.EOL, home: typeof os.homedir(), tmp: typeof os.tmpdir(), cpus: os.cpus().length > 0,
            platform: os.platform() === process.platform, host: typeof os.hostname(),
        };
        "#,
    )
    .await;
    assert_eq!(v["join"], json!("/a/c/d.txt"));
    assert_eq!(v["rel"], json!("../../d"));
    assert_eq!(v["resolve"], json!(true));
    assert_eq!(v["parse"], json!({"root": "/", "dir": "/home/u", "base": "file.tar.gz", "ext": ".gz", "name": "file.tar"}));
    assert_eq!(v["base"], json!("y"));
    assert_eq!(v["ext"], json!(".json"));
    assert_eq!(v["norm"], json!("a/c/"));
    assert_eq!(v["dir"], json!("/a/b"));
    assert_eq!(v["abs"], json!(true));
    assert_eq!(v["eol"], json!("\n"));
    assert_eq!(v["cpus"], json!(true));
    assert_eq!(v["platform"], json!(true));
}

#[tokio::test]
async fn fs_sync_and_async() {
    let dir = tempfile::tempdir().unwrap();
    let rt = rt_in(dir.path());
    let v = eval_module(
        &rt,
        r#"
        import fs, { existsSync, readFileSync, writeFileSync } from "node:fs";
        import { readFile, writeFile, mkdir, readdir, stat, rm, rename, appendFile, copyFile } from "node:fs/promises";
        import { promisify } from "node:util";
        writeFileSync("a.txt", "hello");
        fs.appendFileSync("a.txt", " world");
        await mkdir("sub/deep", { recursive: true });
        await writeFile("sub/deep/b.json", JSON.stringify({ x: 1 }));
        await appendFile("sub/deep/b.json", "\n");
        await copyFile("a.txt", "sub/c.txt");
        await rename("sub/c.txt", "sub/d.txt");
        const st = await stat("a.txt");
        const entries = (await readdir("sub", { withFileTypes: true })).map((d) => [d.name, d.isDirectory()]);
        const rec = (await readdir(".", { recursive: true })).sort();
        let enoent = null;
        try { readFileSync("missing.txt"); } catch (e) { enoent = [e.code, e.syscall ?? null, e.message]; }
        let enoent2 = null;
        await readFile("missing.txt").catch((e) => { enoent2 = e.code; });
        const viaCallback = await promisify(fs.readFile)("a.txt", "utf8");
        const buf = readFileSync("a.txt");
        const fd = fs.openSync("fd.txt", "w");
        fs.writeSync(fd, "via fd");
        fs.closeSync(fd);
        const fdText = readFileSync("fd.txt", "utf8");
        await rm("sub", { recursive: true, force: true });
        const lstat = fs.lstatSync("a.txt");
        const tmp = fs.mkdtempSync(process.cwd() + "/tmp-");
        export default {
            text: readFileSync("a.txt", "utf8"),
            exists: [existsSync("a.txt"), existsSync("nope"), existsSync("sub")],
            size: st.size, isFile: st.isFile(), isDir: st.isDirectory(), mtime: st.mtime instanceof Date,
            entries, rec, enoent, enoent2, viaCallback,
            isBuffer: Buffer.isBuffer(buf), bufLen: buf.length, fdText,
            lstatFile: lstat.isFile(), tmp: existsSync(tmp),
        };
        "#,
    )
    .await;
    assert_eq!(v["text"], json!("hello world"));
    assert_eq!(v["exists"], json!([true, false, false]));
    assert_eq!(v["size"], json!(11));
    assert_eq!(v["isFile"], json!(true));
    assert_eq!(v["isDir"], json!(false));
    assert_eq!(v["mtime"], json!(true));
    assert_eq!(v["entries"], json!([["d.txt", false], ["deep", true]]));
    assert_eq!(v["rec"], json!(["a.txt", "sub", "sub/d.txt", "sub/deep", "sub/deep/b.json"]));
    assert_eq!(v["enoent"][0], json!("ENOENT"));
    assert!(v["enoent"][2].as_str().unwrap().contains("missing.txt"));
    assert_eq!(v["enoent2"], json!("ENOENT"));
    assert_eq!(v["viaCallback"], json!("hello world"));
    assert_eq!(v["isBuffer"], json!(true));
    assert_eq!(v["bufLen"], json!(11));
    assert_eq!(v["fdText"], json!("via fd"));
    assert_eq!(v["lstatFile"], json!(true));
    assert_eq!(v["tmp"], json!(true));
    assert!(dir.path().join("a.txt").exists());
}

#[tokio::test]
async fn fs_streams_and_watch() {
    let dir = tempfile::tempdir().unwrap();
    let rt = rt_in(dir.path());
    let v = eval_module(
        &rt,
        r#"
        import fs from "node:fs";
        import { pipeline } from "node:stream/promises";
        const ws = fs.createWriteStream("out.txt");
        ws.write("line1\n");
        ws.write(Buffer.from("line2\n"));
        await new Promise((r) => ws.end("line3\n", r));
        const chunks = [];
        for await (const c of fs.createReadStream("out.txt", { encoding: "utf8", highWaterMark: 4 })) chunks.push(c);
        await pipeline(fs.createReadStream("out.txt"), fs.createWriteStream("copy.txt"));
        const changes = [];
        let sawNewFile;
        const seen = new Promise((r) => (sawNewFile = r));
        const w = fs.watch(".", { interval: 50 }, (kind, name) => {
            changes.push(name);
            if (name === "new.txt") sawNewFile();
        });
        fs.writeFileSync("new.txt", "x");
        // Wait for the event itself; the cap only bounds a broken watcher.
        await Promise.race([seen, new Promise((r) => setTimeout(r, 10000))]);
        w.close();
        export default { joined: chunks.join(""), many: chunks.length > 1, copy: fs.readFileSync("copy.txt", "utf8"), sawNew: changes.includes("new.txt") };
        "#,
    )
    .await;
    assert_eq!(v["joined"], json!("line1\nline2\nline3\n"));
    assert_eq!(v["many"], json!(true));
    assert_eq!(v["copy"], json!("line1\nline2\nline3\n"));
    assert_eq!(v["sawNew"], json!(true));
}

#[tokio::test]
async fn child_process_apis() {
    let dir = tempfile::tempdir().unwrap();
    let rt = rt_in(dir.path());
    let v = eval_module(
        &rt,
        r#"
        import { spawn, exec, execSync, execFile, spawnSync, execFileSync } from "node:child_process";
        import { promisify } from "node:util";
        const spawned = await new Promise((resolve, reject) => {
            const child = spawn("sh", ["-c", "cat; echo err >&2; exit 3"], { env: { ...process.env, FOO: "bar" } });
            let out = "", err = "";
            child.stdout.setEncoding("utf8");
            child.stdout.on("data", (d) => (out += d));
            child.stderr.on("data", (d) => (err += d.toString()));
            child.on("error", reject);
            let exitSeen = null;
            child.on("exit", (code) => { exitSeen = code; });
            child.on("close", (code, signal) => resolve({ out, err, code, signal, exitSeen, pid: typeof child.pid }));
            child.stdin.write("from stdin\n");
            child.stdin.end();
        });
        const envOut = execSync("echo $FOO", { env: { FOO: "baz", PATH: process.env.PATH }, encoding: "utf8" });
        const { stdout } = await promisify(exec)("printf 'a b'");
        let execErr = null;
        await new Promise((r) => exec("exit 7", (e) => { execErr = e?.code; r(); }));
        const ef = await new Promise((r) => execFile("echo", ["x", "y"], (e, so) => r(so)));
        const ss = spawnSync("sh", ["-c", "read v; echo got:$v"], { input: "hi\n", encoding: "utf8" });
        let syncFail = null;
        try { execSync("echo boom >&2; exit 2", { stdio: "pipe" }); } catch (e) { syncFail = [e.status, e.stderr.toString().trim()]; }
        const missing = await new Promise((r) => { const c = spawn("definitely-not-a-binary-xyz"); c.on("error", (e) => r(e.code)); });
        const killed = await new Promise((r) => { const c = spawn("sleep", ["5"]); c.on("exit", (code, sig) => r(sig)); setTimeout(() => c.kill(), 20); });
        const cwdOut = execFileSync("pwd", { encoding: "utf8" }).trim();
        export default { spawned, envOut, stdout, execErr, ef, ss: [ss.status, ss.stdout], syncFail, missing, killed, cwdOk: cwdOut === process.cwd() };
        "#,
    )
    .await;
    assert_eq!(v["spawned"]["out"], json!("from stdin\n"));
    assert_eq!(v["spawned"]["err"], json!("err\n"));
    assert_eq!(v["spawned"]["code"], json!(3));
    assert_eq!(v["spawned"]["exitSeen"], json!(3));
    assert_eq!(v["spawned"]["signal"], json!(null));
    assert_eq!(v["spawned"]["pid"], json!("number"));
    assert_eq!(v["envOut"], json!("baz\n"));
    assert_eq!(v["stdout"], json!("a b"));
    assert_eq!(v["execErr"], json!(7));
    assert_eq!(v["ef"], json!("x y\n"));
    assert_eq!(v["ss"], json!([0, "got:hi\n"]));
    assert_eq!(v["syncFail"], json!([2, "boom"]));
    assert_eq!(v["missing"], json!("ENOENT"));
    assert_eq!(v["killed"], json!("SIGTERM"));
    assert_eq!(v["cwdOk"], json!(true));
}

#[tokio::test]
async fn events_util_buffer_crypto() {
    let dir = tempfile::tempdir().unwrap();
    let rt = rt_in(dir.path());
    let v = eval_module(
        &rt,
        r#"
        import { EventEmitter, once } from "node:events";
        import util from "node:util";
        import { Buffer } from "node:buffer";
        import crypto, { createHash, randomUUID } from "node:crypto";
        import assert from "node:assert";
        import { StringDecoder } from "node:string_decoder";
        import { setTimeout as sleep } from "node:timers/promises";
        import { fileURLToPath, pathToFileURL } from "node:url";
        import { createRequire } from "node:module";
        const e = new EventEmitter();
        const seen = [];
        e.on("x", (a) => seen.push("on:" + a));
        e.once("x", (a) => seen.push("once:" + a));
        e.prependListener("x", () => seen.push("first"));
        e.emit("x", 1); e.emit("x", 2);
        const p = once(e, "ready"); e.emit("ready", 42); const [ready] = await p;
        class Sub extends EventEmitter {}
        const s = new Sub(); let got; s.on("y", (v) => (got = v)); s.emit("y", "sub");
        let threw = null; try { e.emit("error", new Error("unhandled")); } catch (err) { threw = err.message; }
        const b = Buffer.from("héllo");
        const sd = new StringDecoder("utf8");
        const euro = Buffer.from("€");
        const decoded = sd.write(euro.subarray(0, 1)) + sd.write(euro.subarray(1)) + sd.end();
        assert.deepStrictEqual({ a: [1, { b: 2 }] }, { a: [1, { b: 2 }] });
        let assertMsg = null; try { assert.strictEqual(1, 2); } catch (err) { assertMsg = err.code; }
        await sleep(5);
        const req = createRequire(import.meta.url);
        export default {
            seen, ready, got, threw,
            fmt: util.format("%s=%d %j %o", "a", 4.5, { k: [1] }, [1]),
            insp: util.inspect({ a: { b: { c: { d: 1 } } } }, { depth: 0 }),
            b64: b.toString("base64"), hex: Buffer.from("abc").toString("hex"), len: b.length, byteLen: Buffer.byteLength("héllo"),
            fromB64: Buffer.from("aMOpbGxv", "base64").toString(), concat: Buffer.concat([Buffer.from("a"), Buffer.from("b")]).toString(),
            slice: b.slice(1, 3).toString("hex"), eq: Buffer.from("x").equals(Buffer.from("x")),
            u32: (() => { const x = Buffer.alloc(4); x.writeUInt32BE(0xdeadbeef); return x.readUInt32BE(0).toString(16); })(),
            json: JSON.stringify(Buffer.from([1, 2])),
            sha256: createHash("sha256").update("abc").digest("hex"),
            md5: crypto.createHash("md5").update("a").update("bc").digest("base64"),
            sha1: crypto.createHash("sha1").update(Buffer.from("abc")).digest("hex"),
            hmac: crypto.createHmac("sha256", "key").update("The quick brown fox jumps over the lazy dog").digest("hex"),
            uuid: /^[0-9a-f-]{36}$/.test(randomUUID()), rb: crypto.randomBytes(8).length,
            decoded, assertMsg,
            f2p: fileURLToPath("file:///tmp/a%20b.txt"), p2f: pathToFileURL("/tmp/a b.txt").href,
            types: [util.types.isPromise(Promise.resolve()), util.types.isDate(new Date())],
            reqPath: typeof req("node:path").join,
            isDeep: util.isDeepStrictEqual([1, 2], [1, 2]),
        };
        "#,
    )
    .await;
    assert_eq!(v["seen"], json!(["first", "on:1", "once:1", "first", "on:2"]));
    assert_eq!(v["ready"], json!(42));
    assert_eq!(v["got"], json!("sub"));
    assert_eq!(v["threw"], json!("unhandled"));
    assert_eq!(v["fmt"], json!("a=4.5 {\"k\":[1]} [ 1, [length]: 1 ]"));
    assert_eq!(v["insp"], json!("{ a: [Object] }"));
    assert_eq!(v["b64"], json!("aMOpbGxv"));
    assert_eq!(v["hex"], json!("616263"));
    assert_eq!(v["len"], json!(6));
    assert_eq!(v["byteLen"], json!(6));
    assert_eq!(v["fromB64"], json!("héllo"));
    assert_eq!(v["concat"], json!("ab"));
    assert_eq!(v["slice"], json!("c3a9"));
    assert_eq!(v["eq"], json!(true));
    assert_eq!(v["u32"], json!("deadbeef"));
    assert_eq!(v["json"], json!("{\"type\":\"Buffer\",\"data\":[1,2]}"));
    assert_eq!(v["sha256"], json!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
    assert_eq!(v["md5"], json!("kAFQmDzST7DWlj99KOF/cg=="));
    assert_eq!(v["sha1"], json!("a9993e364706816aba3e25717850c26c9cd0d89d"));
    assert_eq!(v["hmac"], json!("f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"));
    assert_eq!(v["uuid"], json!(true));
    assert_eq!(v["rb"], json!(8));
    assert_eq!(v["decoded"], json!("€"));
    assert_eq!(v["assertMsg"], json!("ERR_ASSERTION"));
    assert_eq!(v["f2p"], json!("/tmp/a b.txt"));
    assert_eq!(v["p2f"], json!("file:///tmp/a%20b.txt"));
    assert_eq!(v["types"], json!([true, true]));
    assert_eq!(v["reqPath"], json!("function"));
    assert_eq!(v["isDeep"], json!(true));
}

#[tokio::test]
async fn process_object() {
    let dir = tempfile::tempdir().unwrap();
    let out = Arc::new(Mutex::new(String::new()));
    let o = out.clone();
    let exits = Arc::new(Mutex::new(Vec::new()));
    let ex = exits.clone();
    let mut env = std::collections::HashMap::new();
    env.insert("MY_VAR".to_string(), "value".to_string());
    let rt = JsRuntime::new(RuntimeOptions {
        cwd: Some(dir.path().to_path_buf()),
        env: Some(env),
        argv: vec!["pi".into(), "ext.ts".into()],
        stdout: Some(Arc::new(move |b: &[u8]| o.lock().push_str(&String::from_utf8_lossy(b)))),
        on_exit: Some(Arc::new(move |c| ex.lock().push(c))),
        ..Default::default()
    })
    .unwrap();
    let v = rt
        .eval(
            r#"
            process.stdout.write("direct\n");
            const hr = process.hrtime();
            const ticks = [];
            process.nextTick(() => ticks.push("tick"));
            let exitErr = null;
            try { process.exit(5); } catch (e) { exitErr = e.code; }
            process.chdir("..");
            ({
                env: process.env.MY_VAR, path: process.env.PATH ?? null, argv: process.argv,
                platform: process.platform, hr: Array.isArray(hr) && hr.length === 2,
                cwdChanged: process.cwd() !== null, pid: typeof process.pid, version: process.version.startsWith("v"),
                exitErr, mem: typeof process.memoryUsage().rss, global: global === globalThis,
            })
            "#,
        )
        .await
        .unwrap();
    assert_eq!(v["env"], json!("value"));
    assert_eq!(v["path"], json!(null));
    assert_eq!(v["argv"], json!(["pi", "ext.ts"]));
    let platform=match std::env::consts::OS { "macos"=>"darwin", "windows"=>"win32", other=>other };
    assert_eq!(v["platform"], json!(platform));
    assert_eq!(v["hr"], json!(true));
    assert_eq!(v["exitErr"], json!("ERR_PROCESS_EXIT"));
    assert_eq!(v["global"], json!(true));
    assert_eq!(out.lock().as_str(), "direct\n");
    assert_eq!(exits.lock().clone(), vec![5]);
}

async fn serve(listener: tokio::net::TcpListener) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((mut sock, _)) = listener.accept().await else { return };
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            let mut req = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                req.extend_from_slice(&buf[..n]);
                if let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&req[..end]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                        .unwrap_or(0);
                    while req.len() < end + 4 + len {
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        req.extend_from_slice(&buf[..n]);
                    }
                    let body = String::from_utf8_lossy(&req[end + 4..]).to_string();
                    let first = head.lines().next().unwrap_or("").to_string();
                    let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
                    if path == "/stream" {
                        // This server closes every connection after one response. Without
                        // `connection: close` the client pools this one and can reuse it for the
                        // next request just as it closes, failing that fetch with a TypeError.
                        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n").await;
                        for i in 0..3 {
                            let chunk = format!("part{i};");
                            let _ = sock.write_all(format!("{:x}\r\n{chunk}\r\n", chunk.len()).as_bytes()).await;
                            let _ = sock.flush().await;
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        }
                        let _ = sock.write_all(b"0\r\n\r\n").await;
                        return;
                    }
                    if path == "/slow" {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                    let ua = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("x-test:").map(|v| v.trim().to_string()))
                        .unwrap_or_default();
                    let payload = serde_json::json!({ "request": first, "body": body, "xtest": ua }).to_string();
                    let resp = format!(
                        "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\nx-reply: yes\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    return;
                }
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_against_local_server() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener));
    let rt = JsRuntime::new(RuntimeOptions::default()).unwrap();
    let code = format!(
        r#"(async () => {{
            const base = "http://{addr}";
            const r = await fetch(base + "/echo?q=1", {{ method: "POST", headers: {{ "X-Test": "hdr" }}, body: JSON.stringify({{ a: 1 }}) }});
            const j = await r.json();
            const s = await fetch(base + "/stream");
            const parts = [];
            const reader = s.body.getReader();
            for (;;) {{ const {{ value, done }} = await reader.read(); if (done) break; parts.push(new TextDecoder().decode(value)); }}
            const ac = new AbortController();
            const slow = fetch(base + "/slow", {{ signal: ac.signal }});
            setTimeout(() => ac.abort(), 30);
            let abortName = null;
            try {{ await slow; }} catch (e) {{ abortName = e.name; }}
            let netErr = null;
            try {{ await fetch("http://127.0.0.1:1/"); }} catch (e) {{ netErr = e instanceof TypeError; }}
            const h = new Headers({{ "Content-Type": "a" }}); h.append("x-multi", "1"); h.append("x-multi", "2");
            const resp = new Response("body text", {{ status: 404 }});
            return {{
                status: r.status, ok: r.ok, reply: r.headers.get("x-reply"), ct: r.headers.get("content-type"),
                request: j.request, body: j.body, xtest: j.xtest,
                streamed: parts.join(""), chunks: parts.length, abortName, netErr,
                multi: h.get("x-multi"), respText: await resp.text(), respOk: resp.ok,
                dataUrl: await (await fetch("data:text/plain;base64,aGk=")).text(),
            }};
        }})()"#
    );
    let v = rt.eval(code).await.unwrap();
    assert_eq!(v["status"], json!(201));
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["reply"], json!("yes"));
    assert_eq!(v["ct"], json!("application/json"));
    assert_eq!(v["request"], json!("POST /echo?q=1 HTTP/1.1"));
    assert_eq!(v["body"], json!("{\"a\":1}"));
    assert_eq!(v["xtest"], json!("hdr"));
    assert_eq!(v["streamed"], json!("part0;part1;part2;"));
    assert!(v["chunks"].as_u64().unwrap() >= 2, "{v}");
    assert_eq!(v["abortName"], json!("AbortError"));
    assert_eq!(v["netErr"], json!(true));
    assert_eq!(v["multi"], json!("1, 2"));
    assert_eq!(v["respText"], json!("body text"));
    assert_eq!(v["respOk"], json!(false));
    assert_eq!(v["dataUrl"], json!("hi"));
}

#[tokio::test]
async fn readline_and_streams() {
    let dir = tempfile::tempdir().unwrap();
    let rt = rt_in(dir.path());
    let v = eval_module(
        &rt,
        r#"
        import { Readable, Transform, PassThrough, Writable } from "node:stream";
        import readline from "node:readline";
        import { spawn } from "node:child_process";
        const upper = new Transform({ transform(chunk, enc, cb) { cb(null, chunk.toString().toUpperCase()); } });
        const collected = [];
        const sink = new Writable({ write(chunk, enc, cb) { collected.push(chunk.toString()); cb(); } });
        await new Promise((r) => { Readable.from(["a", "b", "c"]).pipe(upper).pipe(sink).on("finish", r); });
        const child = spawn("printf", ["one\\ntwo\\nthree\\n"]);
        const rl = readline.createInterface({ input: child.stdout });
        const lines = [];
        for await (const line of rl) lines.push(line);
        const pt = new PassThrough();
        pt.end("pass");
        let ptOut = "";
        for await (const c of pt) ptOut += c;
        export default { collected: collected.join(""), lines, ptOut };
        "#,
    )
    .await;
    assert_eq!(v["collected"], json!("ABC"));
    assert_eq!(v["lines"], json!(["one", "two", "three"]));
    assert_eq!(v["ptOut"], json!("pass"));
}
