//! `node:dgram` UDP sockets: bind/send/connect/disconnect/close state machine, callbacks and
//! errors. The expected output is Node 22's for the same script (run by `pi-jsrt-node`).

use std::process::Command;

const NODE_BIN: &str = env!("CARGO_BIN_EXE_pi-jsrt-node");

#[test]
fn udp_sockets_match_node() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dg.js");
    std::fs::write(&path, SCRIPT).unwrap();
    let out = Command::new(NODE_BIN).arg(&path).output().unwrap();
    assert_eq!(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)), EXPECTED);
}

const SCRIPT: &str = r###"const dgram = require("dgram");
const log = [];
const a = dgram.createSocket("udp4");
const b = dgram.createSocket({ type: "udp4", reuseAddr: true });
try { a.address(); } catch (e) { log.push(["address-unbound", e.code, e.message]); }
try { dgram.createSocket("udp5"); } catch (e) { log.push(["bad type", e.code, e.message]); }
a.on("message", (msg, rinfo) => {
  log.push(["a got", msg.toString(), rinfo.address, rinfo.family, typeof rinfo.port, rinfo.size]);
  a.send(Buffer.from("pong"), rinfo.port, rinfo.address, (err, bytes) => log.push(["a sent", err, bytes]));
});
a.on("listening", () => {
  const { address, family, port } = a.address();
  log.push(["a listening", address, family, port > 0]);
  b.on("message", (msg, rinfo) => {
    log.push(["b got", msg.toString(), rinfo.size]);
    try { a.bind(0); } catch (e) { log.push(["rebind", e.code, e.message]); }
    b.connect(port, "127.0.0.1", () => {
      log.push(["b connected", b.remoteAddress().port === port]);
      try { b.send("x", port, "127.0.0.1"); } catch (e) { log.push(["send connected with port", e.code, e.message]); }
      b.send("via connect", (err) => {
        log.push(["b sent connected", err]);
        a.once("message", (m) => {
          log.push(["a got 2", m.toString()]);
          b.disconnect();
          try { b.remoteAddress(); } catch (e) { log.push(["disconnected", e.code, e.message]); }
          a.close(() => {
            log.push(["a closed"]);
            try { a.send("x", 1, "127.0.0.1"); } catch (e) { log.push(["send after close", e.code, e.message]); }
            b.close();
            console.log(JSON.stringify(log));
          });
        });
      });
    });
  });
  b.send(["pi", "ng"], port, "localhost", (err, bytes) => log.push(["b sent", err, bytes]));
});
a.bind(0, "127.0.0.1");
"###;
const EXPECTED: &str = r###"[["address-unbound","EBADF","getsockname EBADF"],["bad type","ERR_SOCKET_BAD_TYPE","Bad socket type specified. Valid types are: udp4, udp6"],["a listening","127.0.0.1","IPv4",true],["b sent",null,4],["a got","ping","127.0.0.1","IPv4","number",4],["a sent",null,4],["b got","pong",4],["rebind","ERR_SOCKET_ALREADY_BOUND","Socket is already bound"],["b connected",true],["b sent connected",null],["a got","x","127.0.0.1","IPv4","number",1],["a got 2","x"],["disconnected","ERR_SOCKET_DGRAM_NOT_CONNECTED","Not connected"],["a closed"],["send after close","ERR_SOCKET_DGRAM_NOT_RUNNING","Not running"]]
"###;
