//! `node:cluster` (round-robin and shared-handle scheduling) and handle passing over the IPC
//! channel (`subprocess.send(message, socket | server)`). Expected outputs are Node 22's; with
//! `node` on PATH, handle passing is also checked between Node and pi-jsrt in both directions.

use std::process::Command;

const NODE_BIN: &str = env!("CARGO_BIN_EXE_pi-jsrt-node");

fn run(program: &str, script: &std::path::Path, env: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(program);
    cmd.arg(script);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

#[test]
fn cluster_round_robin_and_shared_handles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cl.js");
    std::fs::write(&path, CLUSTER).unwrap();
    assert_eq!(run(NODE_BIN, &path, &[]), CLUSTER_NODE);
    // SCHED_NONE: the kernel picks the accepting worker, so only check the run completes.
    let none = run(NODE_BIN, &path, &[("POLICY", "none")]);
    assert!(none.contains(r#"["online",2]"#) && none.contains(r#"["exit",2,0,null,true]"#), "{none}");
}

#[test]
fn sockets_and_servers_pass_between_processes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hp.js");
    std::fs::write(&path, HANDLES).unwrap();
    assert_eq!(run(NODE_BIN, &path, &[]), HANDLES_NODE);
    let Ok(node) = Command::new("node").arg("-e").arg("process.stdout.write(process.execPath)").output() else { return };
    if !node.status.success() {
        return;
    }
    let node = String::from_utf8_lossy(&node.stdout).into_owned();
    assert_eq!(run(&node, &path, &[("CHILD_EXEC", NODE_BIN)]), HANDLES_NODE);
    assert_eq!(run(NODE_BIN, &path, &[("CHILD_EXEC", &node)]), HANDLES_NODE);
}

const CLUSTER: &str = r###"const cluster = require("cluster");
const http = require("http");
if (cluster.isPrimary) {
  const log = [];
  const policy = process.env.POLICY;
  if (policy === "none") cluster.schedulingPolicy = cluster.SCHED_NONE;
  cluster.on("fork", (w) => log.push(["fork", w.id]));
  cluster.on("online", (w) => log.push(["online", w.id]));
  let listening = 0;
  cluster.on("listening", (w, addr) => {
    log.push(["listening", w.id, typeof addr.port, addr.addressType]);
    if (++listening < 2) return;
    let done = 0;
    const pids = new Set();
    for (let i = 0; i < 6; i++) {
      http.get(`http://127.0.0.1:${addr.port}/`, (res) => {
        let b = ""; res.on("data", (d) => b += d);
        res.on("end", () => {
          pids.add(b);
          if (++done === 6) {
            log.push(["distinct workers served", pids.size]);
            for (const w of Object.values(cluster.workers)) w.send({ bye: true });
          }
        });
      });
    }
  });
  cluster.on("message", (w, m) => log.push(["message", w.id, m]));
  cluster.on("exit", (w, code, sig) => {
    log.push(["exit", w.id, code, sig, w.exitedAfterDisconnect]);
    if (Object.keys(cluster.workers).length === 0) console.log(JSON.stringify(log.sort((a, b) => JSON.stringify(a) < JSON.stringify(b) ? -1 : 1)));
  });
  cluster.fork({ X: "1" });
  cluster.fork({ X: "2" });
} else {
  const server = http.createServer((req, res) => { res.setHeader("Connection", "close"); res.end(String(cluster.worker.id)); });
  server.listen(0, "127.0.0.1");
  process.send({ hello: cluster.worker.id, x: process.env.X, isWorker: cluster.isWorker, uid: process.env.NODE_UNIQUE_ID });
  process.on("message", (m) => { if (m.bye) { server.close(); cluster.worker.disconnect(); } });
}
"###;
const CLUSTER_NODE: &str = r###"[["distinct workers served",2],["exit",1,0,null,true],["exit",2,0,null,true],["fork",1],["fork",2],["listening",1,"number",4],["listening",2,"number",4],["message",1,{"hello":1,"x":"1","isWorker":true}],["message",2,{"hello":2,"x":"2","isWorker":true}],["online",1],["online",2]]
"###;
const HANDLES: &str = r###"const { fork } = require("child_process");
const net = require("net");
if (process.argv[2] === "child") {
  process.on("message", (m, handle) => {
    if (m === "socket") { handle.end(`handled by child ${typeof handle.remotePort}\n`); }
    if (m === "server") { handle.on("connection", (s) => s.end("child server\n")); process.send("server ready"); }
  });
} else {
  const child = fork(__filename, ["child"], process.env.CHILD_EXEC ? { execPath: process.env.CHILD_EXEC } : {});
  const out = [];
  const server = net.createServer({ pauseOnConnect: true }, (socket) => child.send("socket", socket));
  server.listen(0, "127.0.0.1", () => {
    const c = net.connect(server.address().port, "127.0.0.1");
    let d = ""; c.on("data", (x) => d += x);
    c.on("end", () => {
      out.push(d);
      const s2 = net.createServer();
      s2.listen(0, "127.0.0.1", () => {
        const port = s2.address().port;
        child.once("message", () => {
          s2.close();
          const c2 = net.connect(port, "127.0.0.1");
          let d2 = ""; c2.on("data", (x) => d2 += x);
          c2.on("end", () => { out.push(d2); console.log(JSON.stringify(out)); child.kill(); server.close(); });
        });
        child.send("server", s2);
      });
    });
  });
}
"###;
const HANDLES_NODE: &str = r###"["handled by child number\n","child server\n"]
"###;
