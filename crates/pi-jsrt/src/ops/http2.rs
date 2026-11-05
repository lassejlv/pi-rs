//! HTTP/2 sessions for `node:http2` on the h2 crate, sans I/O like Node's nghttp2 binding: the
//! session runs over an in-memory duplex; JS feeds the bytes it reads from its socket
//! (`h2Feed`) and writes what the session produces (`internal.h2Event(id, "out", bytes)`), so
//! sessions work over `net` and `tls` sockets alike (and over any duplex a user supplies).
//!
//! Streams are addressed by ids allocated here; their events are dispatched through
//! `internal.h2Event(sessionId, kind, ...)` with the stream id as the first payload value.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::js::JsArray;
use crate::js::{Context, JsArgs, JsResult, JsValue, js_string};
use bytes::Bytes;
use h2::{Reason, RecvStream, SendStream};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::{bytes_arg, bytes_to_js};
use crate::engine::{self, OpFn, Poster, arg_f64, arg_string, js_str, state};

static NEXT_STREAM: AtomicU64 = AtomicU64::new(1);

enum Input {
    Data(Vec<u8>),
    End,
}

enum SessionCmd {
    Request { stream: u64, headers: Vec<(String, String)>, end: bool, cmds: mpsc::UnboundedReceiver<StreamCmd> },
    Close,
    Ping(Vec<u8>, u64),
}

enum StreamCmd {
    Respond { headers: Vec<(String, String)>, end: bool },
    Data(Vec<u8>, bool),
    Trailers(Vec<(String, String)>),
    Reset(u32),
}

struct SessionCtl {
    input: mpsc::UnboundedSender<Input>,
    cmds: mpsc::UnboundedSender<SessionCmd>,
}

thread_local! {
    static SESSIONS: RefCell<HashMap<u64, SessionCtl>> = RefCell::new(HashMap::new());
    static STREAMS: RefCell<HashMap<u64, mpsc::UnboundedSender<StreamCmd>>> = RefCell::new(HashMap::new());
}

fn dispatch(ctx: &mut Context, session: u64, args: Vec<JsValue>) {
    let Some(internal) = engine::try_internal(ctx) else { return };
    let Ok(f) = internal.get(js_string!("h2Event"), ctx) else { return };
    let Some(f) = f.as_function() else { return };
    let mut all = vec![JsValue::from(session as f64)];
    all.extend(args);
    if let Err(e) = f.call(&JsValue::undefined(), &all, ctx) {
        engine::report_uncaught(ctx, e);
    }
}

#[derive(Clone)]
struct Events {
    poster: Poster,
    session: u64,
}

impl Events {
    fn post(&self, kind: &'static str, build: impl FnOnce(&mut Context) -> Vec<JsValue> + Send + 'static) {
        let session = self.session;
        self.poster.post(move |ctx| {
            let mut args = vec![js_str(kind)];
            args.extend(build(ctx));
            dispatch(ctx, session, args);
        });
    }

    fn stream_error(&self, stream: u64, e: &h2::Error) {
        let code = e.reason().map(u32::from).unwrap_or(2);
        let msg = e.to_string();
        self.post("streamError", move |_| vec![JsValue::from(stream as f64), JsValue::from(code), js_str(&msg)]);
    }
}

fn headers_to_js(ctx: &mut Context, pseudo: Vec<(&str, String)>, map: &HeaderMap) -> JsValue {
    let mut out = Vec::new();
    for (k, v) in pseudo {
        out.push(js_str(k));
        out.push(js_str(&v));
    }
    for (k, v) in map {
        out.push(js_str(k.as_str()));
        out.push(js_str(&String::from_utf8_lossy(v.as_bytes())));
    }
    super::array(ctx, out)
}

fn header_pairs(v: &JsValue, ctx: &mut Context) -> JsResult<Vec<(String, String)>> {
    let Some(obj) = v.as_object() else { return Ok(Vec::new()) };
    let arr = JsArray::from_object(obj)?;
    let len = arr.length(ctx)?;
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < len {
        let k = arr.get(i, ctx)?.to_string(ctx)?.to_std_string_lossy();
        let v = arr.get(i + 1, ctx)?.to_string(ctx)?.to_std_string_lossy();
        out.push((k, v));
        i += 2;
    }
    Ok(out)
}

fn header_map(pairs: &[(String, String)]) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    for (k, v) in pairs {
        if k.starts_with(':') {
            continue;
        }
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| format!("Invalid HTTP header name \"{k}\""))?;
        let value = HeaderValue::from_str(v).map_err(|_| format!("Invalid value \"{v}\" for header \"{k}\""))?;
        map.append(name, value);
    }
    Ok(map)
}

fn pseudo<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// Send `data` on `stream` respecting HTTP/2 flow control.
async fn send_all(stream: &mut SendStream<Bytes>, data: Vec<u8>, end: bool) -> Result<(), h2::Error> {
    let mut data = Bytes::from(data);
    if data.is_empty() {
        return stream.send_data(data, end);
    }
    while !data.is_empty() {
        stream.reserve_capacity(data.len());
        let cap = match std::future::poll_fn(|cx| stream.poll_capacity(cx)).await {
            Some(Ok(n)) => n,
            Some(Err(e)) => return Err(e),
            None => return Ok(()),
        };
        if cap == 0 {
            continue;
        }
        let chunk = data.split_to(cap.min(data.len()));
        stream.send_data(chunk, end && data.is_empty())?;
    }
    Ok(())
}

/// Drive a stream's outgoing side from JS commands.
async fn write_stream(mut send: SendStream<Bytes>, mut cmds: mpsc::UnboundedReceiver<StreamCmd>, events: Events, id: u64) {
    while let Some(cmd) = cmds.recv().await {
        let r = match cmd {
            StreamCmd::Data(data, end) => send_all(&mut send, data, end).await,
            StreamCmd::Trailers(headers) => match header_map(&headers) {
                Ok(map) => send.send_trailers(map),
                Err(_) => Ok(()),
            },
            StreamCmd::Reset(code) => {
                send.send_reset(Reason::from(code));
                return;
            }
            StreamCmd::Respond { .. } => Ok(()),
        };
        if let Err(e) = r {
            events.stream_error(id, &e);
            return;
        }
    }
}

/// Deliver a stream's incoming body (data, trailers, end) to JS.
async fn read_body(mut body: RecvStream, events: Events, id: u64) {
    loop {
        match body.data().await {
            Some(Ok(chunk)) => {
                let n = chunk.len();
                let _ = body.flow_control().release_capacity(n);
                let chunk = chunk.to_vec();
                events.post("data", move |ctx| vec![JsValue::from(id as f64), bytes_to_js(&chunk, ctx).unwrap_or_default()]);
            }
            Some(Err(e)) => {
                events.stream_error(id, &e);
                return;
            }
            None => break,
        }
    }
    match body.trailers().await {
        Ok(Some(map)) => events.post("trailers", move |ctx| vec![JsValue::from(id as f64), headers_to_js(ctx, vec![], &map)]),
        Ok(None) => {}
        Err(e) => {
            events.stream_error(id, &e);
            return;
        }
    }
    events.post("end", move |_| vec![JsValue::from(id as f64)]);
}

/// Pump bytes between JS and the in-memory side of the session.
fn start_pumps(tokio: &tokio::runtime::Handle, outer: tokio::io::DuplexStream, events: Events) -> mpsc::UnboundedSender<Input> {
    let (mut reader, mut writer) = tokio::io::split(outer);
    let (tx, mut rx) = mpsc::unbounded_channel::<Input>();
    tokio.spawn(async move {
        while let Some(input) = rx.recv().await {
            match input {
                Input::Data(d) => {
                    if writer.write_all(&d).await.is_err() {
                        return;
                    }
                }
                Input::End => {
                    let _ = writer.shutdown().await;
                    return;
                }
            }
        }
    });
    tokio.spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    events.post("out", move |ctx| vec![bytes_to_js(&chunk, ctx).unwrap_or_default()]);
                }
            }
        }
    });
    tx
}

fn settings_from(o: &JsValue, ctx: &mut Context) -> JsResult<Vec<(String, f64)>> {
    let mut out = Vec::new();
    if let Some(obj) = o.as_object() {
        for key in ["headerTableSize", "enablePush", "initialWindowSize", "maxFrameSize", "maxConcurrentStreams", "maxHeaderListSize", "enableConnectProtocol"] {
            let v = obj.get(js_str(key).as_string().unwrap().clone(), ctx)?;
            if !v.is_undefined() {
                let n = if v.is_boolean() { if v.to_boolean() { 1.0 } else { 0.0 } } else { v.to_number(ctx)? };
                out.push((key.to_string(), n));
            }
        }
    }
    Ok(out)
}

/// `ops.h2Session(kind, settings) -> id`: kind is "client" or "server".
fn op_h2_session(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let server = arg_string(args, 0, ctx)? == "server";
    let settings = settings_from(args.get_or_undefined(1), ctx)?;
    let st = state(ctx);
    let id = st.next_id();
    let events = Events { poster: st.poster(), session: id };
    let (inner, outer) = tokio::io::duplex(256 * 1024);
    let input = start_pumps(&st.tokio, outer, events.clone());
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();
    SESSIONS.with(|s| s.borrow_mut().insert(id, SessionCtl { input, cmds: cmd_tx }));
    if server {
        st.tokio.spawn(run_server(inner, settings, cmd_rx, events));
    } else {
        st.tokio.spawn(run_client(inner, settings, cmd_rx, events));
    }
    Ok(JsValue::from(id as f64))
}

macro_rules! apply_settings {
    ($builder:expr, $settings:expr) => {
        for (k, v) in $settings {
            let n = *v as u32;
            match k.as_str() {
                "headerTableSize" => {
                    $builder.header_table_size(n);
                }
                "initialWindowSize" => {
                    $builder.initial_window_size(n);
                }
                "maxFrameSize" => {
                    $builder.max_frame_size(n.clamp(16384, 16_777_215));
                }
                "maxConcurrentStreams" => {
                    $builder.max_concurrent_streams(n);
                }
                "maxHeaderListSize" => {
                    $builder.max_header_list_size(n);
                }
                _ => {}
            }
        }
    };
}

async fn run_client(io: tokio::io::DuplexStream, settings: Vec<(String, f64)>, mut cmds: mpsc::UnboundedReceiver<SessionCmd>, events: Events) {
    let mut builder = h2::client::Builder::new();
    apply_settings!(builder, &settings);
    if let Some((_, v)) = settings.iter().find(|(k, _)| k == "enablePush") {
        builder.enable_push(*v != 0.0);
    }
    let (send, mut conn) = match builder.handshake::<_, Bytes>(io).await {
        Ok(v) => v,
        Err(e) => {
            let msg = e.to_string();
            events.post("error", move |_| vec![js_str(&msg)]);
            return;
        }
    };
    let mut ping = conn.ping_pong();
    let conn_events = events.clone();
    let driver = tokio::spawn(async move {
        let r = conn.await;
        let (code, msg) = match &r {
            Ok(()) => (0, String::new()),
            Err(e) => (e.reason().map(u32::from).unwrap_or(2), e.to_string()),
        };
        let go_away = r.as_ref().err().is_some_and(|e| e.is_go_away() && e.is_remote());
        conn_events.post("close", move |_| vec![JsValue::from(code), js_str(&msg), JsValue::from(go_away)]);
    });
    let mut send = Some(send);
    while let Some(cmd) = cmds.recv().await {
        match cmd {
            SessionCmd::Request { stream, headers, end, cmds: stream_cmds } => {
                let Some(sender) = send.as_ref() else { continue };
                let mut sender = sender.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    let method = pseudo(&headers, ":method").unwrap_or("GET").to_string();
                    let scheme = pseudo(&headers, ":scheme").unwrap_or("http").to_string();
                    let authority = pseudo(&headers, ":authority").unwrap_or("localhost").to_string();
                    let path = pseudo(&headers, ":path").unwrap_or("/").to_string();
                    let built = (|| -> Result<Request<()>, String> {
                        let uri = format!("{scheme}://{authority}{path}");
                        let mut req = Request::builder()
                            .method(Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?)
                            .uri(uri)
                            .body(())
                            .map_err(|e| e.to_string())?;
                        *req.headers_mut() = header_map(&headers)?;
                        Ok(req)
                    })();
                    let req = match built {
                        Ok(r) => r,
                        Err(msg) => {
                            events.post("streamError", move |_| vec![JsValue::from(stream as f64), JsValue::from(1), js_str(&msg)]);
                            return;
                        }
                    };
                    if let Err(e) = std::future::poll_fn(|cx| sender.poll_ready(cx)).await {
                        events.stream_error(stream, &e);
                        return;
                    }
                    let (response, send_stream) = match sender.send_request(req, end) {
                        Ok(v) => v,
                        Err(e) => {
                            events.stream_error(stream, &e);
                            return;
                        }
                    };
                    let h2_id = u32::from(send_stream.stream_id());
                    events.post("ready", move |_| vec![JsValue::from(stream as f64), JsValue::from(h2_id)]);
                    tokio::spawn(write_stream(send_stream, stream_cmds, events.clone(), stream));
                    match response.await {
                        Ok(resp) => {
                            let status = resp.status().as_u16().to_string();
                            let (parts, body) = resp.into_parts();
                            let end_stream = body.is_end_stream();
                            events.post("response", move |ctx| {
                                vec![JsValue::from(stream as f64), headers_to_js(ctx, vec![(":status", status)], &parts.headers), JsValue::from(end_stream)]
                            });
                            read_body(body, events, stream).await;
                        }
                        Err(e) => events.stream_error(stream, &e),
                    }
                });
            }
            SessionCmd::Close => {
                // GOAWAY once the open streams are done: no new requests.
                send = None;
            }
            SessionCmd::Ping(payload, cb) => {
                let mut bytes = [0u8; 8];
                bytes[..payload.len().min(8)].copy_from_slice(&payload[..payload.len().min(8)]);
                let events = events.clone();
                if let Some(mut p) = ping.take() {
                    let started = std::time::Instant::now();
                    let r = p.ping(h2::Ping::opaque()).await;
                    let ms = started.elapsed().as_secs_f64() * 1000.0;
                    let ok = r.is_ok();
                    events.post("ping", move |ctx| vec![JsValue::from(cb as f64), JsValue::from(ok), JsValue::from(ms), bytes_to_js(&bytes, ctx).unwrap_or_default()]);
                    ping = Some(p);
                } else {
                    events.post("ping", move |ctx| vec![JsValue::from(cb as f64), JsValue::from(false), JsValue::from(0), bytes_to_js(&bytes, ctx).unwrap_or_default()]);
                }
            }
        }
    }
    drop(send);
    let _ = driver.await;
}

async fn run_server(io: tokio::io::DuplexStream, settings: Vec<(String, f64)>, mut cmds: mpsc::UnboundedReceiver<SessionCmd>, events: Events) {
    let mut builder = h2::server::Builder::new();
    apply_settings!(builder, &settings);
    let mut conn = match builder.handshake::<_, Bytes>(io).await {
        Ok(c) => c,
        Err(e) => {
            let msg = e.to_string();
            events.post("error", move |_| vec![js_str(&msg)]);
            return;
        }
    };
    let mut ping = conn.ping_pong();
    let mut closing = false;
    let result = loop {
        tokio::select! {
            accepted = conn.accept() => match accepted {
                Some(Ok((req, respond))) => {
                    let id = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
                    let (parts, body) = req.into_parts();
                    let end_stream = body.is_end_stream();
                    let mut pseudo_headers = vec![(":method", parts.method.to_string())];
                    if let Some(a) = parts.uri.authority() {
                        pseudo_headers.push((":authority", a.to_string()));
                    }
                    pseudo_headers.push((":scheme", parts.uri.scheme_str().unwrap_or("http").to_string()));
                    pseudo_headers.push((":path", parts.uri.path_and_query().map(|p| p.to_string()).unwrap_or_else(|| "/".into())));
                    let h2_id = u32::from(respond.stream_id());
                    let (tx, rx) = mpsc::unbounded_channel::<StreamCmd>();
                    let headers = parts.headers;
                    events.post("stream", move |ctx| {
                        STREAMS.with(|s| s.borrow_mut().insert(id, tx));
                        vec![JsValue::from(id as f64), headers_to_js(ctx, pseudo_headers, &headers), JsValue::from(end_stream), JsValue::from(h2_id)]
                    });
                    tokio::spawn(serve_stream(respond, rx, events.clone(), id));
                    tokio::spawn(read_body(body, events.clone(), id));
                }
                Some(Err(e)) => break Err(e),
                None => break Ok(()),
            },
            cmd = cmds.recv(), if !closing => match cmd {
                Some(SessionCmd::Close) | None => {
                    closing = true;
                    conn.graceful_shutdown();
                }
                Some(SessionCmd::Ping(payload, cb)) => {
                    let mut bytes = [0u8; 8];
                    bytes[..payload.len().min(8)].copy_from_slice(&payload[..payload.len().min(8)]);
                    let ok = ping.as_mut().is_some();
                    events.post("ping", move |ctx| vec![JsValue::from(cb as f64), JsValue::from(ok), JsValue::from(0), bytes_to_js(&bytes, ctx).unwrap_or_default()]);
                }
                Some(SessionCmd::Request { .. }) => {}
            },
        }
    };
    let (code, msg) = match &result {
        Ok(()) => (0, String::new()),
        Err(e) => (e.reason().map(u32::from).unwrap_or(2), e.to_string()),
    };
    let go_away = result.as_ref().err().is_some_and(|e| e.is_go_away() && e.is_remote());
    events.post("close", move |_| vec![JsValue::from(code), js_str(&msg), JsValue::from(go_away)]);
}

/// A server stream: wait for `respond`, then write data/trailers.
async fn serve_stream(mut respond: h2::server::SendResponse<Bytes>, mut cmds: mpsc::UnboundedReceiver<StreamCmd>, events: Events, id: u64) {
    while let Some(cmd) = cmds.recv().await {
        match cmd {
            StreamCmd::Respond { headers, end } => {
                let status = pseudo(&headers, ":status").and_then(|s| s.parse::<u16>().ok()).unwrap_or(200);
                let built = (|| -> Result<Response<()>, String> {
                    let mut resp = Response::builder().status(StatusCode::from_u16(status).map_err(|e| e.to_string())?).body(()).map_err(|e| e.to_string())?;
                    *resp.headers_mut() = header_map(&headers)?;
                    Ok(resp)
                })();
                let resp = match built {
                    Ok(r) => r,
                    Err(msg) => {
                        respond.send_reset(Reason::INTERNAL_ERROR);
                        events.post("streamError", move |_| vec![JsValue::from(id as f64), JsValue::from(2), js_str(&msg)]);
                        return;
                    }
                };
                match respond.send_response(resp, end) {
                    Ok(send) => {
                        if !end {
                            write_stream(send, cmds, events, id).await;
                        }
                        return;
                    }
                    Err(e) => {
                        events.stream_error(id, &e);
                        return;
                    }
                }
            }
            StreamCmd::Reset(code) => {
                respond.send_reset(Reason::from(code));
                return;
            }
            StreamCmd::Data(..) | StreamCmd::Trailers(_) => {}
        }
    }
}

/// `ops.h2Feed(session, bytes)`
fn op_h2_feed(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let data = bytes_arg(args.get_or_undefined(1), ctx)?;
    SESSIONS.with(|s| {
        if let Some(c) = s.borrow().get(&id) {
            let _ = c.input.send(Input::Data(data));
        }
    });
    Ok(JsValue::undefined())
}

/// `ops.h2FeedEnd(session)`: the socket ended.
fn op_h2_feed_end(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    SESSIONS.with(|s| {
        if let Some(c) = s.borrow().get(&id) {
            let _ = c.input.send(Input::End);
        }
    });
    Ok(JsValue::undefined())
}

/// `ops.h2Request(session, headerPairs, endStream) -> stream id`
fn op_h2_request(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let session = arg_f64(args, 0, ctx)? as u64;
    let headers = header_pairs(args.get_or_undefined(1), ctx)?;
    let end = args.get_or_undefined(2).to_boolean();
    let stream = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::unbounded_channel::<StreamCmd>();
    STREAMS.with(|s| s.borrow_mut().insert(stream, tx));
    SESSIONS.with(|s| {
        if let Some(c) = s.borrow().get(&session) {
            let _ = c.cmds.send(SessionCmd::Request { stream, headers, end, cmds: rx });
        }
    });
    Ok(JsValue::from(stream as f64))
}

fn stream_cmd(id: u64, cmd: StreamCmd) -> bool {
    STREAMS.with(|s| s.borrow().get(&id).is_some_and(|tx| tx.send(cmd).is_ok()))
}

/// `ops.h2Respond(stream, headerPairs, endStream)`
fn op_h2_respond(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let headers = header_pairs(args.get_or_undefined(1), ctx)?;
    let end = args.get_or_undefined(2).to_boolean();
    Ok(JsValue::from(stream_cmd(id, StreamCmd::Respond { headers, end })))
}

/// `ops.h2Write(stream, bytes, endStream)`
fn op_h2_write(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let data = bytes_arg(args.get_or_undefined(1), ctx)?;
    let end = args.get_or_undefined(2).to_boolean();
    Ok(JsValue::from(stream_cmd(id, StreamCmd::Data(data, end))))
}

/// `ops.h2Trailers(stream, headerPairs)`
fn op_h2_trailers(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let headers = header_pairs(args.get_or_undefined(1), ctx)?;
    Ok(JsValue::from(stream_cmd(id, StreamCmd::Trailers(headers))))
}

/// `ops.h2Reset(stream, code)`
fn op_h2_reset(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let code = arg_f64(args, 1, ctx)? as u32;
    Ok(JsValue::from(stream_cmd(id, StreamCmd::Reset(code))))
}

/// `ops.h2StreamDone(stream)`: JS is finished with the stream.
fn op_h2_stream_done(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    STREAMS.with(|s| s.borrow_mut().remove(&id));
    Ok(JsValue::undefined())
}

/// `ops.h2Close(session)`: graceful close (GOAWAY after open streams).
fn op_h2_close(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    SESSIONS.with(|s| {
        if let Some(c) = s.borrow().get(&id) {
            let _ = c.cmds.send(SessionCmd::Close);
        }
    });
    Ok(JsValue::undefined())
}

/// `ops.h2Ping(session, payload, callbackId)`
fn op_h2_ping(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let payload = bytes_arg(args.get_or_undefined(1), ctx)?;
    let cb = arg_f64(args, 2, ctx)? as u64;
    let ok = SESSIONS.with(|s| s.borrow().get(&id).is_some_and(|c| c.cmds.send(SessionCmd::Ping(payload, cb)).is_ok()));
    Ok(JsValue::from(ok))
}

/// `ops.h2Destroy(session)`: drop the session (its tasks end with the input).
fn op_h2_destroy(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    if let Some(c) = SESSIONS.with(|s| s.borrow_mut().remove(&id)) {
        let _ = c.input.send(Input::End);
    }
    Ok(JsValue::undefined())
}

pub(crate) fn teardown(_ctx: &mut Context) {
    SESSIONS.with(|s| s.borrow_mut().clear());
    STREAMS.with(|s| s.borrow_mut().clear());
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("h2Session", 2, op_h2_session),
    ("h2Feed", 2, op_h2_feed),
    ("h2FeedEnd", 1, op_h2_feed_end),
    ("h2Request", 3, op_h2_request),
    ("h2Respond", 3, op_h2_respond),
    ("h2Write", 3, op_h2_write),
    ("h2Trailers", 2, op_h2_trailers),
    ("h2Reset", 2, op_h2_reset),
    ("h2StreamDone", 1, op_h2_stream_done),
    ("h2Close", 1, op_h2_close),
    ("h2Ping", 3, op_h2_ping),
    ("h2Destroy", 1, op_h2_destroy),
];
