//! WebSocket client ops backing the global `WebSocket` (`js/web/websocket.js`).
//!
//! Connections run on tokio (tokio-tungstenite over rustls with the default trust store).
//! Lifecycle events and messages are posted to the JS thread and dispatched through
//! `internal.wsEvent(id, kind, ...)`.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::js::{Context, JsArgs, JsResult, JsValue, js_string};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Bytes, Message};

use super::{bytes_arg, bytes_to_js};
use crate::engine::{self, OpFn, arg_f64, arg_string, js_str, state};

enum Out {
    Text(String),
    Binary(Vec<u8>),
    Close(Option<(u16, String)>),
}

thread_local! {
    static SOCKETS: RefCell<HashMap<u64, mpsc::UnboundedSender<Out>>> = RefCell::new(HashMap::new());
}

fn dispatch(ctx: &mut Context, id: u64, args: Vec<JsValue>) {
    let Some(internal) = engine::try_internal(ctx) else { return };
    let Ok(f) = internal.get(js_string!("wsEvent"), ctx) else { return };
    let Some(f) = f.as_function() else { return };
    let mut all = vec![JsValue::from(id as f64)];
    all.extend(args);
    if let Err(e) = f.call(&JsValue::undefined(), &all, ctx) {
        engine::report_uncaught(ctx, e);
    }
}

fn string_list(v: &JsValue, ctx: &mut Context) -> JsResult<Vec<String>> {
    let Some(obj) = v.as_object() else { return Ok(Vec::new()) };
    let arr = crate::js::JsArray::from_object(obj)?;
    let mut out = Vec::new();
    for i in 0..arr.length(ctx)? {
        out.push(arr.get(i, ctx)?.to_string(ctx)?.to_std_string_lossy());
    }
    Ok(out)
}

/// `ops.wsConnect(url, protocols[], headerPairs[]) -> id`
fn op_ws_connect(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let url = arg_string(args, 0, ctx)?;
    let protocols = string_list(args.get_or_undefined(1), ctx)?;
    let headers = string_list(args.get_or_undefined(2), ctx)?;
    let st = state(ctx);
    let id = st.next_id();
    let poster = st.poster();
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    SOCKETS.with(|s| s.borrow_mut().insert(id, tx));
    st.loop_ref(1);
    st.tokio.spawn(async move {
        let fail = |poster: &engine::Poster| {
            poster.post(move |ctx| {
                SOCKETS.with(|s| s.borrow_mut().remove(&id));
                state(ctx).loop_ref(-1);
                dispatch(ctx, id, vec![js_str("fail")]);
            });
        };
        let mut request = match url.as_str().into_client_request() {
            Ok(r) => r,
            Err(_) => return fail(&poster),
        };
        {
            let h = request.headers_mut();
            if !protocols.is_empty()
                && let Ok(v) = HeaderValue::from_str(&protocols.join(", "))
            {
                h.insert("sec-websocket-protocol", v);
            }
            for pair in headers.chunks(2) {
                if let [k, v] = pair
                    && let (Ok(k), Ok(v)) = (
                        tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(k.as_bytes()),
                        HeaderValue::from_str(v),
                    )
                {
                    h.insert(k, v);
                }
            }
            if !h.contains_key("user-agent") {
                h.insert("user-agent", HeaderValue::from_static("node"));
            }
        }
        let connector = match super::tls::default_client_config(vec![]) {
            Ok(c) => tokio_tungstenite::Connector::Rustls(c),
            Err(_) => return fail(&poster),
        };
        let connected = tokio_tungstenite::connect_async_tls_with_config(request, None, true, Some(connector)).await;
        let (stream, response) = match connected {
            Ok(v) => v,
            Err(_) => return fail(&poster),
        };
        let protocol = response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let extensions = response
            .headers()
            .get("sec-websocket-extensions")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        poster.post(move |ctx| dispatch(ctx, id, vec![js_str("open"), js_str(&protocol), js_str(&extensions)]));
        let (mut sink, mut source) = stream.split();
        let write_poster = poster.clone();
        let writer = tokio::spawn(async move {
            while let Some(out) = rx.recv().await {
                let (msg, n) = match out {
                    Out::Text(t) => {
                        let n = t.len();
                        (Message::Text(t.into()), n)
                    }
                    Out::Binary(b) => {
                        let n = b.len();
                        (Message::Binary(Bytes::from(b)), n)
                    }
                    Out::Close(frame) => (
                        Message::Close(frame.map(|(code, reason)| CloseFrame { code: CloseCode::from(code), reason: reason.into() })),
                        0,
                    ),
                };
                let closing = matches!(msg, Message::Close(_));
                if sink.send(msg).await.is_err() {
                    break;
                }
                if n > 0 {
                    write_poster.post(move |ctx| dispatch(ctx, id, vec![js_str("sent"), JsValue::from(n as f64)]));
                }
                if closing {
                    break;
                }
            }
            sink
        });
        let mut close: Option<(u16, String)> = None;
        let mut clean = false;
        while let Some(item) = source.next().await {
            match item {
                Ok(Message::Text(t)) => {
                    let text = t.as_str().to_string();
                    poster.post(move |ctx| dispatch(ctx, id, vec![js_str("text"), js_str(&text)]));
                }
                Ok(Message::Binary(b)) => {
                    let data = b.to_vec();
                    poster.post(move |ctx| {
                        if let Ok(v) = bytes_to_js(&data, ctx) {
                            dispatch(ctx, id, vec![js_str("binary"), v]);
                        }
                    });
                }
                Ok(Message::Close(frame)) => {
                    clean = true;
                    close = Some(match frame {
                        Some(f) => (u16::from(f.code), f.reason.as_str().to_string()),
                        None => (1005, String::new()),
                    });
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        writer.abort();
        let (code, reason) = if clean { close.unwrap_or((1005, String::new())) } else { (1006, String::new()) };
        poster.post(move |ctx| {
            SOCKETS.with(|s| s.borrow_mut().remove(&id));
            state(ctx).loop_ref(-1);
            dispatch(ctx, id, vec![js_str("close"), JsValue::from(code), js_str(&reason), JsValue::from(clean)]);
        });
    });
    Ok(JsValue::from(id as f64))
}

/// `ops.wsSend(id, data, binary)`
fn op_ws_send(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let binary = args.get_or_undefined(2).to_boolean();
    let out = if binary {
        Out::Binary(bytes_arg(args.get_or_undefined(1), ctx)?)
    } else {
        Out::Text(arg_string(args, 1, ctx)?)
    };
    SOCKETS.with(|s| {
        if let Some(tx) = s.borrow().get(&id) {
            let _ = tx.send(out);
        }
    });
    Ok(JsValue::undefined())
}

/// `ops.wsClose(id, code?, reason?)`
fn op_ws_close(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let code = args.get_or_undefined(1);
    let frame = if code.is_undefined() {
        None
    } else {
        let code = code.to_number(ctx)? as u16;
        let reason = args.get_or_undefined(2);
        let reason = if reason.is_undefined() { String::new() } else { reason.to_string(ctx)?.to_std_string_lossy() };
        Some((code, reason))
    };
    SOCKETS.with(|s| {
        if let Some(tx) = s.borrow().get(&id) {
            let _ = tx.send(Out::Close(frame));
        }
    });
    Ok(JsValue::undefined())
}

pub(crate) fn teardown(_ctx: &mut Context) {
    SOCKETS.with(|s| s.borrow_mut().clear());
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[("wsConnect", 3, op_ws_connect), ("wsSend", 3, op_ws_send), ("wsClose", 3, op_ws_close)];
