//! HTTP ops backing `fetch` (via reqwest), with streaming response bodies and abort.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use crate::js::JsArray;
use crate::js::{Context, JsArgs, JsResult, JsValue};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::{array, bytes_arg, bytes_to_js, object};
use crate::engine::{self, OpFn, arg_f64, arg_string, js_str};
use crate::error::{Error, JsException};

type Body = Arc<tokio::sync::Mutex<Option<reqwest::Response>>>;

static BODIES: LazyLock<Mutex<HashMap<u64, Body>>> = LazyLock::new(Default::default);
static TOKENS: LazyLock<Mutex<HashMap<u64, CancellationToken>>> = LazyLock::new(Default::default);

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(20))
        .build()
        .unwrap_or_default()
});
static CLIENT_NO_REDIRECT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
});

fn fetch_error(msg: String, cause: Option<String>) -> Error {
    let message = match cause {
        Some(c) => format!("{msg}: {c}"),
        None => msg,
    };
    Error::Js(JsException {
        name: "TypeError".into(),
        message,
        stack: None,
        code: None,
    })
}

fn abort_error() -> Error {
    Error::Js(JsException {
        name: "AbortError".into(),
        message: "This operation was aborted".into(),
        stack: None,
        code: Some("ABORT_ERR".into()),
    })
}

fn token(id: u64) -> CancellationToken {
    TOKENS.lock().entry(id).or_default().clone()
}

struct Head {
    status: u16,
    status_text: String,
    url: String,
    headers: Vec<(String, String)>,
}

/// `ops.fetchStart(id, method, url, headerPairs, body|null, redirect)`
fn op_fetch_start(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let method = arg_string(args, 1, ctx)?;
    let url = arg_string(args, 2, ctx)?;
    let mut headers = Vec::new();
    if let Some(obj) = args.get_or_undefined(3).as_object() {
        let arr = JsArray::from_object(obj)?;
        let len = arr.length(ctx)?;
        let mut i = 0;
        while i + 1 < len {
            let k = arr.get(i, ctx)?.to_string(ctx)?.to_std_string_lossy();
            let v = arr.get(i + 1, ctx)?.to_string(ctx)?.to_std_string_lossy();
            headers.push((k, v));
            i += 2;
        }
    }
    let body_v = args.get_or_undefined(4);
    let body = if body_v.is_null_or_undefined() { None } else { Some(bytes_arg(body_v, ctx)?) };
    let redirect = arg_string(args, 5, ctx).unwrap_or_else(|_| "follow".into());
    let cancel = token(id);
    let fut = async move {
        let method = reqwest::Method::from_bytes(method.to_ascii_uppercase().as_bytes())
            .map_err(|_| fetch_error(format!("'{method}' is not a valid HTTP method."), None))?;
        let client = if redirect == "follow" { &*CLIENT } else { &*CLIENT_NO_REDIRECT };
        let mut req = client.request(method, &url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        let resp = tokio::select! {
            r = req.send() => r.map_err(|e| {
                let cause = std::error::Error::source(&e).map(|s| s.to_string());
                fetch_error("fetch failed".into(), cause.or_else(|| Some(e.to_string())))
            })?,
            _ = cancel.cancelled() => return Err(abort_error()),
        };
        if redirect == "error" && resp.status().is_redirection() {
            return Err(fetch_error("fetch failed".into(), Some("unexpected redirect".into())));
        }
        let head = Head {
            status: resp.status().as_u16(),
            status_text: resp.status().canonical_reason().unwrap_or("").to_string(),
            url: resp.url().to_string(),
            headers: resp
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
                .collect(),
        };
        BODIES.lock().insert(id, Arc::new(tokio::sync::Mutex::new(Some(resp))));
        Ok(head)
    };
    Ok(engine::spawn_result(ctx, fut, move |head: Head, ctx| {
        let mut flat = Vec::new();
        for (k, v) in &head.headers {
            flat.push(js_str(k));
            flat.push(js_str(v));
        }
        let headers = array(ctx, flat);
        object(
            ctx,
            vec![
                ("status", JsValue::from(head.status)),
                ("statusText", js_str(&head.status_text)),
                ("url", js_str(&head.url)),
                ("headers", headers),
            ],
        )
    }))
}

/// `ops.fetchRead(id) -> Promise<Uint8Array | null>`
fn op_fetch_read(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let body = BODIES.lock().get(&id).cloned();
    let cancel = token(id);
    let fut = async move {
        let Some(body) = body else { return Ok(None) };
        let mut guard = body.lock().await;
        let Some(resp) = guard.as_mut() else { return Ok(None) };
        let chunk = tokio::select! {
            c = resp.chunk() => c.map_err(|e| fetch_error("terminated".into(), Some(e.to_string())))?,
            _ = cancel.cancelled() => return Err(abort_error()),
        };
        match chunk {
            Some(c) => Ok(Some(c.to_vec())),
            None => {
                *guard = None;
                drop(guard);
                BODIES.lock().remove(&id);
                TOKENS.lock().remove(&id);
                Ok(None)
            }
        }
    };
    Ok(engine::spawn_result(ctx, fut, |chunk: Option<Vec<u8>>, ctx| match chunk {
        Some(c) => bytes_to_js(&c, ctx),
        None => Ok(JsValue::null()),
    }))
}

/// `ops.fetchCancel(id)`: abort an in-flight request / body and free it.
fn op_fetch_cancel(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    if let Some(t) = TOKENS.lock().remove(&id) {
        t.cancel();
    }
    BODIES.lock().remove(&id);
    Ok(JsValue::undefined())
}

fn op_fetch_id(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(engine::state(ctx).next_id() as f64))
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("fetchId", 0, op_fetch_id),
    ("fetchStart", 6, op_fetch_start),
    ("fetchRead", 1, op_fetch_read),
    ("fetchCancel", 1, op_fetch_cancel),
];
