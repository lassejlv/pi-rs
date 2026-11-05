//! The global `WebSocket` against a loopback tokio-tungstenite server. The expected values
//! are Node 22's output for the same script against an equivalent server.

use futures::{SinkExt, StreamExt};
use pi_jsrt::{JsRuntime, RuntimeOptions};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;

/// Echo server: `/hello` greets first, `/reject` answers 403, "close-me" makes the server close
/// with 4001 "bye", "drop" drops the TCP connection; the last offered subprotocol is selected.
#[allow(clippy::result_large_err)]
async fn server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut path = String::new();
                let cb = |req: &Request, mut res: Response| -> Result<Response, ErrorResponse> {
                    path = req.uri().path().to_string();
                    if path == "/reject" {
                        let mut r = ErrorResponse::new(None);
                        *r.status_mut() = StatusCode::FORBIDDEN;
                        return Err(r);
                    }
                    if let Some(p) = req.headers().get("sec-websocket-protocol").and_then(|v| v.to_str().ok()) {
                        let last = p.split(',').map(str::trim).next_back().unwrap_or("").to_string();
                        res.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_str(&last).unwrap());
                    }
                    Ok(res)
                };
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tcp, cb).await else { return };
                if path == "/hello" {
                    let _ = ws.send(Message::Text("hello from server".into())).await;
                }
                while let Some(Ok(msg)) = ws.next().await {
                    match msg {
                        Message::Text(t) if t.as_str() == "close-me" => {
                            let _ = ws.close(Some(CloseFrame { code: CloseCode::from(4001), reason: "bye".into() })).await;
                        }
                        Message::Text(t) if t.as_str() == "drop" => return,
                        Message::Text(_) | Message::Binary(_) if ws.send(msg.clone()).await.is_err() => return,
                        _ => {}
                    }
                }
            });
        }
    });
    port
}

const CLIENT: &str = r#"
const port = process.env.WS_PORT;
const log = [];
const done = (ws) => new Promise((r) => ws.addEventListener("close", r));
{
  const ws = new WebSocket(`ws://127.0.0.1:${port}/hello`, ["chat", "superchat"]);
  log.push(["init", ws.readyState, ws.url, ws.protocol, ws.binaryType, ws.bufferedAmount]);
  ws.binaryType = "arraybuffer";
  ws.onopen = (e) => { log.push(["open", e.type, e.constructor.name, ws.readyState, ws.protocol, ws.extensions]); ws.send("text"); ws.send(new Uint8Array([1, 2, 3])); ws.send(new Uint8Array([9,8,7,6]).buffer); ws.send(new Blob(["blobby"])); };
  let n = 0;
  ws.onmessage = (e) => {
    log.push(["msg", e.constructor.name, typeof e.data, e.data instanceof ArrayBuffer ? Array.from(new Uint8Array(e.data)) : e.data, e.origin]);
    if (++n === 5) ws.close(4000, "done");
  };
  ws.onclose = (e) => log.push(["close", e.constructor.name, e.code, e.reason, e.wasClean, ws.readyState]);
  await done(ws);
}
{
  const ws = new WebSocket(`ws://127.0.0.1:${port}/x`);
  ws.onopen = () => ws.send("close-me");
  ws.onclose = (e) => log.push(["server-close", e.code, e.reason, e.wasClean]);
  await done(ws);
}
{
  const ws = new WebSocket(`ws://127.0.0.1:${port}/x`);
  ws.onopen = () => { ws.send(new Uint8Array(3)); ws.send("drop"); };
  ws.onmessage = async (e) => log.push(["blob", e.data.constructor.name, e.data.size, Array.from(new Uint8Array(await e.data.arrayBuffer()))]);
  ws.onerror = (e) => log.push(["drop-error", e.type, e.message]);
  ws.onclose = (e) => log.push(["drop-close", e.code, e.reason, e.wasClean]);
  await done(ws);
}
{
  const ws = new WebSocket(`ws://127.0.0.1:${port}/reject`);
  ws.onerror = (e) => log.push(["reject-error", e.constructor.name, e.message, ws.readyState]);
  ws.onclose = (e) => log.push(["reject-close", e.code]);
  await new Promise((r) => setTimeout(r, 300));
}
{
  const ws = new WebSocket(`http://127.0.0.1:${port}/x`);
  log.push(["http-url", ws.url]);
  ws.onopen = () => ws.close();
  ws.onclose = (e) => log.push(["http-close", e.code, e.reason, e.wasClean]);
  await done(ws);
}
const errs = [];
for (const f of [() => new WebSocket("ftp://x"), () => new WebSocket("/rel"), () => new WebSocket("ws://x/#frag"), () => new WebSocket("ws://x", ["a", "a"]), () => new WebSocket("ws://x", "a b"), () => { const w = new WebSocket(`ws://127.0.0.1:${port}/`); w.onerror = () => {}; w.send("x"); }, () => { const w = new WebSocket(`ws://127.0.0.1:${port}/`); w.close(1001); }, () => { const w = new WebSocket(`ws://127.0.0.1:${port}/`); w.close(3000, "é".repeat(62)); }]) {
  try { f(); errs.push("no error"); } catch (e) { errs.push([e.name, e.message, e.code]); }
}
log.push(["errs", errs]);
log.push(["statics", WebSocket.CONNECTING, WebSocket.OPEN, WebSocket.CLOSING, WebSocket.CLOSED, Object.prototype.toString.call(new WebSocket(`ws://127.0.0.1:${port}/`))]);
export default JSON.stringify(log).replaceAll(port, "PORT");
"#;

/// Node 22.22's output for `CLIENT`.
const NODE: &str = r#"[["init",0,"ws://127.0.0.1:PORT/hello","","blob",0],["open","open","Event",1,"superchat",""],["msg","MessageEvent","string","hello from server","ws://127.0.0.1:PORT"],["msg","MessageEvent","string","text","ws://127.0.0.1:PORT"],["msg","MessageEvent","object",[1,2,3],"ws://127.0.0.1:PORT"],["msg","MessageEvent","object",[9,8,7,6],"ws://127.0.0.1:PORT"],["msg","MessageEvent","object",[98,108,111,98,98,121],"ws://127.0.0.1:PORT"],["close","CloseEvent",4000,"done",true,3],["server-close",4001,"bye",true],["blob","Blob",3,[0,0,0]],["drop-close",1006,"",false],["reject-error","ErrorEvent","Received network error or non-101 status code.",0],["http-url","ws://127.0.0.1:PORT/x"],["http-close",1005,"",true],["errs",[["SyntaxError","Expected a ws: or wss: protocol, got ftp:",12],["SyntaxError","TypeError: Invalid URL",12],["SyntaxError","Got fragment",12],["SyntaxError","Invalid Sec-WebSocket-Protocol value",12],["SyntaxError","Invalid Sec-WebSocket-Protocol value",12],["InvalidStateError","Sent before connected.",11],["InvalidAccessError","invalid code",15],["SyntaxError","Reason must be less than 123 bytes; received 124",12]]],["statics",0,1,2,3,"[object WebSocket]"]]"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_matches_node() {
    let port = server().await;
    let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
    env.insert("WS_PORT".into(), port.to_string());
    let rt = JsRuntime::new(RuntimeOptions { env: Some(env), ..Default::default() }).unwrap();
    let ns = tokio::time::timeout(std::time::Duration::from_secs(60), rt.eval_module(CLIENT, "ws-test.mjs"))
        .await
        .expect("timed out")
        .unwrap_or_else(|e| panic!("{e}\n{:?}", e.as_js().and_then(|j| j.stack.clone())));
    let out = ns.get("default").await.unwrap();
    assert_eq!(out.as_str().unwrap(), NODE);
}
