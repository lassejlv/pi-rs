//! TCP ops backing `node:net` (and the HTTP server/client built on it in `js/node/http.js`).
//!
//! Listeners and sockets run on tokio. Accepted connections, received data and lifecycle
//! events are posted back to the JS thread and dispatched through
//! `internal.netEvent(id, kind, ...)`; writes from JS go to a per-socket writer task.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};

use crate::js::{Context, JsArgs, JsResult, JsValue, js_string};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{bytes_arg, bytes_to_js, io_code, object};
use crate::engine::{self, OpFn, Poster, arg_f64, arg_string, js_str, state};

enum SockMsg {
    Data(Vec<u8>),
    End,
    /// The descriptor was handed to another process: stop without shutting the stream down.
    Detach,
}

struct SocketCtl {
    tx: mpsc::UnboundedSender<SockMsg>,
    stop: CancellationToken,
    /// The stream's descriptor (owned by the reader/writer tasks), for handle passing.
    fd: i32,
}

struct ServerCtl {
    stop: CancellationToken,
    fd: i32,
}

thread_local! {
    static SOCKETS: RefCell<HashMap<u64, SocketCtl>> = RefCell::new(HashMap::new());
    static SERVERS: RefCell<HashMap<u64, ServerCtl>> = RefCell::new(HashMap::new());
}

fn dispatch(ctx: &mut Context, id: u64, args: Vec<JsValue>) {
    let Some(internal) = engine::try_internal(ctx) else { return };
    let Ok(f) = internal.get(js_string!("netEvent"), ctx) else { return };
    let Some(f) = f.as_function() else { return };
    let mut all = vec![JsValue::from(id as f64)];
    all.extend(args);
    if let Err(e) = f.call(&JsValue::undefined(), &all, ctx) {
        engine::report_uncaught(ctx, e);
    }
}

fn family(addr: &SocketAddr) -> &'static str {
    if addr.is_ipv4() { "IPv4" } else { "IPv6" }
}

/// `{ localAddress, localPort, localFamily, remoteAddress, remotePort, remoteFamily }`
fn endpoints(ctx: &mut Context, local: Option<SocketAddr>, remote: Option<SocketAddr>) -> JsResult<JsValue> {
    let mut entries = Vec::new();
    if let Some(l) = local {
        entries.push(("localAddress", js_str(&l.ip().to_string())));
        entries.push(("localPort", JsValue::from(l.port())));
        entries.push(("localFamily", js_str(family(&l))));
    }
    if let Some(r) = remote {
        entries.push(("remoteAddress", js_str(&r.ip().to_string())));
        entries.push(("remotePort", JsValue::from(r.port())));
        entries.push(("remoteFamily", js_str(family(&r))));
    }
    object(ctx, entries)
}

fn error_object(ctx: &mut Context, e: &std::io::Error, syscall: &str, address: Option<(&str, u16)>) -> JsResult<JsValue> {
    let code = io_code(e);
    let mut message = format!("{syscall} {code}");
    let mut entries = vec![
        ("code", js_str(code)),
        ("errno", JsValue::from(-e.raw_os_error().unwrap_or(0))),
        ("syscall", js_str(syscall)),
    ];
    if let Some((host, port)) = address {
        message.push_str(&format!(" {host}:{port}"));
        entries.push(("address", js_str(host)));
        entries.push(("port", JsValue::from(port)));
    }
    entries.insert(0, ("message", js_str(&message)));
    object(ctx, entries)
}

/// Start the reader and writer tasks of a connected stream and register it under `id`.
fn start_socket(tokio: &tokio::runtime::Handle, poster: Poster, id: u64, stream: tokio::net::TcpStream) -> SocketCtl {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let (tx, mut rx) = mpsc::unbounded_channel::<SockMsg>();
    let stop = CancellationToken::new();
    let (mut reader, mut writer) = stream.into_split();
    let read_poster = poster.clone();
    let read_stop = stop.clone();
    let read_task = tokio.spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = tokio::select! {
                _ = read_stop.cancelled() => return false,
                r = reader.read(&mut buf) => r,
            };
            match n {
                Ok(0) => {
                    read_poster.post(move |ctx| dispatch(ctx, id, vec![js_str("end")]));
                    return false;
                }
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    read_poster.post(move |ctx| {
                        if let Ok(b) = bytes_to_js(&chunk, ctx) {
                            dispatch(ctx, id, vec![js_str("data"), b]);
                        }
                    });
                }
                Err(e) => {
                    read_poster.post(move |ctx| {
                        if let Ok(err) = error_object(ctx, &e, "read", None) {
                            dispatch(ctx, id, vec![js_str("error"), err]);
                        }
                    });
                    return true;
                }
            }
        }
    });
    let write_poster = poster.clone();
    let write_stop = stop.clone();
    let write_task = tokio.spawn(async move {
        loop {
            let msg = tokio::select! {
                _ = write_stop.cancelled() => return false,
                m = rx.recv() => m,
            };
            match msg {
                Some(SockMsg::Data(data)) => {
                    if let Err(e) = writer.write_all(&data).await {
                        write_poster.post(move |ctx| {
                            if let Ok(err) = error_object(ctx, &e, "write", None) {
                                dispatch(ctx, id, vec![js_str("error"), err]);
                            }
                        });
                        return true;
                    }
                }
                Some(SockMsg::Detach) => {
                    // Dropping a write half shuts the socket down for every process sharing it.
                    writer.forget();
                    write_stop.cancel();
                    return false;
                }
                Some(SockMsg::End) | None => {
                    let _ = writer.shutdown().await;
                    return false;
                }
            }
        }
    });
    tokio.spawn(async move {
        // `close` follows both directions: the peer ended (or an error) and JS ended its side
        // (`end()`), or `destroy()` stopped both.
        let read_error = read_task.await.unwrap_or(false);
        let write_error = write_task.await.unwrap_or(false);
        let had_error = read_error || write_error;
        poster.post(move |ctx| {
            SOCKETS.with(|s| s.borrow_mut().remove(&id));
            dispatch(ctx, id, vec![js_str("close"), JsValue::from(had_error)]);
        });
    });
    SocketCtl { tx, stop, fd }
}

/// `ops.netListen(host, port) -> { id, address, port, family } | { error }`
fn op_net_listen(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let host = arg_string(args, 0, ctx)?;
    let port = arg_f64(args, 1, ctx)? as u16;
    let host = if host.is_empty() { "0.0.0.0".to_string() } else { host };
    let bound = (host.as_str(), port)
        .to_socket_addrs()
        .and_then(|mut addrs| addrs.next().ok_or_else(|| std::io::Error::from(std::io::ErrorKind::AddrNotAvailable)))
        .and_then(std::net::TcpListener::bind);
    let listener = match bound {
        Ok(l) => l,
        Err(e) => {
            let err = error_object(ctx, &e, "listen", Some((&host, port)))?;
            return object(ctx, vec![("error", err)]);
        }
    };
    let raw = args.get_or_undefined(2).to_boolean();
    serve(ctx, listener, raw, &host, port)
}

/// Accept connections on `listener`: each becomes a socket ("connection" events), or with `raw`
/// a bare descriptor handed to JS ("rawconnection" events), as cluster's round-robin primary
/// passes connections to workers.
fn serve(ctx: &mut Context, listener: std::net::TcpListener, raw: bool, host: &str, port: u16) -> JsResult<JsValue> {
    use std::os::fd::{AsRawFd, IntoRawFd};
    let local = listener.local_addr().ok();
    let st = state(ctx);
    if let Err(e) = listener.set_nonblocking(true) {
        let err = error_object(ctx, &e, "listen", Some((host, port)))?;
        return object(ctx, vec![("error", err)]);
    }
    let fd = listener.as_raw_fd();
    let listener = {
        let _guard = st.tokio.enter();
        match tokio::net::TcpListener::from_std(listener) {
            Ok(l) => l,
            Err(e) => {
                let err = error_object(ctx, &e, "listen", Some((host, port)))?;
                return object(ctx, vec![("error", err)]);
            }
        }
    };
    let id = st.next_id();
    let stop = CancellationToken::new();
    SERVERS.with(|s| s.borrow_mut().insert(id, ServerCtl { stop: stop.clone(), fd }));
    let poster = st.poster();
    let tokio = st.tokio.clone();
    let shared = st.shared.clone();
    st.tokio.spawn(async move {
        loop {
            let accepted = tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                a = listener.accept() => a,
            };
            let Ok((stream, remote)) = accepted else { continue };
            let _ = stream.set_nodelay(true);
            let local = stream.local_addr().ok();
            if raw {
                let Ok(std_stream) = stream.into_std() else { continue };
                let raw_fd = std_stream.into_raw_fd();
                poster.post(move |ctx| {
                    if let Ok(info) = endpoints(ctx, local, Some(remote)) {
                        dispatch(ctx, id, vec![js_str("rawconnection"), JsValue::from(raw_fd), info]);
                    }
                });
                continue;
            }
            let socket_id = shared.next_id();
            let tokio2 = tokio.clone();
            let poster2 = poster.clone();
            poster.post(move |ctx| {
                let ctl = start_socket(&tokio2, poster2, socket_id, stream);
                SOCKETS.with(|s| s.borrow_mut().insert(socket_id, ctl));
                if let Ok(info) = endpoints(ctx, local, Some(remote)) {
                    dispatch(ctx, id, vec![js_str("connection"), JsValue::from(socket_id as f64), info]);
                }
            });
        }
        drop(listener);
        poster.post(move |ctx| {
            SERVERS.with(|s| s.borrow_mut().remove(&id));
            dispatch(ctx, id, vec![js_str("close")]);
        });
    });
    let local = local.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], port)));
    object(
        ctx,
        vec![
            ("id", JsValue::from(id as f64)),
            ("address", js_str(&local.ip().to_string())),
            ("port", JsValue::from(local.port())),
            ("family", js_str(family(&local))),
        ],
    )
}

/// `ops.netListenFd(fd, raw)`: listen on an inherited/passed listening descriptor (owned from now on).
fn op_net_listen_fd(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use std::os::fd::FromRawFd;
    let fd = arg_f64(args, 0, ctx)? as i32;
    let raw = args.get_or_undefined(1).to_boolean();
    // SAFETY: the descriptor was passed to this runtime, which takes ownership of it.
    let listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
    serve(ctx, listener, raw, "", 0)
}

/// `ops.netAdoptFd(fd) -> { id, ...endpoints } | { error }`: a connected stream from a descriptor.
fn op_net_adopt_fd(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use std::os::fd::FromRawFd;
    let fd = arg_f64(args, 0, ctx)? as i32;
    // SAFETY: the descriptor was passed to this runtime, which takes ownership of it.
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
    let _ = std_stream.set_nonblocking(true);
    let st = state(ctx);
    let stream = {
        let _guard = st.tokio.enter();
        tokio::net::TcpStream::from_std(std_stream)
    };
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            let err = error_object(ctx, &e, "open", None)?;
            return object(ctx, vec![("error", err)]);
        }
    };
    let local = stream.local_addr().ok();
    let remote = stream.peer_addr().ok();
    let id = st.next_id();
    let ctl = start_socket(&st.tokio, st.poster(), id, stream);
    SOCKETS.with(|s| s.borrow_mut().insert(id, ctl));
    let info = endpoints(ctx, local, remote)?;
    if let Some(o) = info.as_object() {
        o.set(js_string!("id"), JsValue::from(id as f64), true, ctx)?;
    }
    Ok(info)
}

/// `ops.netBindFd(host, port) -> { fd, address, port, family } | { error }`: a listening socket
/// that nobody accepts on here (cluster's shared handle: workers listen on its descriptor).
fn op_net_bind_fd(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use std::os::fd::IntoRawFd;
    let host = arg_string(args, 0, ctx)?;
    let port = arg_f64(args, 1, ctx)? as u16;
    let host = if host.is_empty() { "0.0.0.0".to_string() } else { host };
    let bound = (host.as_str(), port)
        .to_socket_addrs()
        .and_then(|mut addrs| addrs.next().ok_or_else(|| std::io::Error::from(std::io::ErrorKind::AddrNotAvailable)))
        .and_then(std::net::TcpListener::bind);
    match bound {
        Ok(l) => {
            let local = l.local_addr().unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], port)));
            let fd = l.into_raw_fd();
            object(
                ctx,
                vec![
                    ("fd", JsValue::from(fd)),
                    ("address", js_str(&local.ip().to_string())),
                    ("port", JsValue::from(local.port())),
                    ("family", js_str(family(&local))),
                ],
            )
        }
        Err(e) => {
            let err = error_object(ctx, &e, "listen", Some((&host, port)))?;
            object(ctx, vec![("error", err)])
        }
    }
}

/// `ops.netFd(id) -> fd | -1`: the descriptor of a socket or server (for handle passing).
fn op_net_fd(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let fd = SOCKETS.with(|s| s.borrow().get(&id).map(|c| c.fd)).or_else(|| SERVERS.with(|s| s.borrow().get(&id).map(|c| c.fd)));
    Ok(JsValue::from(fd.unwrap_or(-1)))
}

/// `ops.netConnect(host, port) -> id`; emits `connect` (with endpoints) or `error`.
fn op_net_connect(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let host = arg_string(args, 0, ctx)?;
    let port = arg_f64(args, 1, ctx)? as u16;
    let host = if host.is_empty() { "localhost".to_string() } else { host };
    let st = state(ctx);
    let id = st.next_id();
    let poster = st.poster();
    let tokio = st.tokio.clone();
    st.tokio.spawn(async move {
        match tokio::net::TcpStream::connect((host.as_str(), port)).await {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                let local = stream.local_addr().ok();
                let remote = stream.peer_addr().ok();
                let poster2 = poster.clone();
                poster.post(move |ctx| {
                    let ctl = start_socket(&tokio, poster2, id, stream);
                    SOCKETS.with(|s| s.borrow_mut().insert(id, ctl));
                    if let Ok(info) = endpoints(ctx, local, remote) {
                        dispatch(ctx, id, vec![js_str("connect"), info]);
                    }
                });
            }
            Err(e) => {
                poster.post(move |ctx| {
                    if let Ok(err) = error_object(ctx, &e, "connect", Some((&host, port))) {
                        dispatch(ctx, id, vec![js_str("error"), err]);
                        dispatch(ctx, id, vec![js_str("close"), JsValue::from(true)]);
                    }
                });
            }
        }
    });
    Ok(JsValue::from(id as f64))
}

fn op_net_write(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let data = bytes_arg(args.get_or_undefined(1), ctx)?;
    let ok = SOCKETS.with(|s| s.borrow().get(&id).is_some_and(|c| c.tx.send(SockMsg::Data(data)).is_ok()));
    Ok(JsValue::from(ok))
}

fn op_net_end(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    SOCKETS.with(|s| {
        if let Some(c) = s.borrow().get(&id) {
            let _ = c.tx.send(SockMsg::End);
        }
    });
    Ok(JsValue::undefined())
}

/// `ops.netDetach(id)`: the socket's descriptor was sent to another process; stop using it here
/// without shutting the connection down.
fn op_net_detach(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    SOCKETS.with(|s| {
        if let Some(c) = s.borrow().get(&id) {
            let _ = c.tx.send(SockMsg::Detach);
        }
    });
    Ok(JsValue::undefined())
}

fn op_net_destroy(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    SOCKETS.with(|s| {
        if let Some(c) = s.borrow().get(&id) {
            c.stop.cancel();
        }
    });
    Ok(JsValue::undefined())
}

fn op_net_server_close(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    SERVERS.with(|s| {
        if let Some(c) = s.borrow().get(&id) {
            c.stop.cancel();
        }
    });
    Ok(JsValue::undefined())
}

/// Close every listener and socket (runtime shutdown).
pub(crate) fn teardown(_ctx: &mut Context) {
    SERVERS.with(|s| s.borrow_mut().drain().for_each(|(_, c)| c.stop.cancel()));
    SOCKETS.with(|s| s.borrow_mut().drain().for_each(|(_, c)| c.stop.cancel()));
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("netListen", 2, op_net_listen),
    ("netConnect", 2, op_net_connect),
    ("netWrite", 2, op_net_write),
    ("netEnd", 1, op_net_end),
    ("netDestroy", 1, op_net_destroy),
    ("netServerClose", 1, op_net_server_close),
    ("netListenFd", 2, op_net_listen_fd),
    ("netAdoptFd", 1, op_net_adopt_fd),
    ("netFd", 1, op_net_fd),
    ("netDetach", 1, op_net_detach),
    ("netBindFd", 2, op_net_bind_fd),
];

/// `ops.dnsLookup(hostname) -> Promise<[{ address, family }]>` (getaddrinfo via tokio).
fn op_dns_lookup(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let host = arg_string(args, 0, ctx)?;
    Ok(engine::spawn_result(
        ctx,
        async move {
            match tokio::net::lookup_host((host.as_str(), 0)).await {
                Ok(addrs) => {
                    let mut seen = Vec::new();
                    for a in addrs {
                        let item = (a.ip().to_string(), if a.is_ipv4() { 4 } else { 6 });
                        if !seen.contains(&item) {
                            seen.push(item);
                        }
                    }
                    Ok(seen)
                }
                Err(e) => {
                    // getaddrinfo's EAI_NONAME / EAI_AGAIN surface as io errors without errno.
                    let code = if e.raw_os_error().is_some() { io_code(&e) } else { "ENOTFOUND" };
                    Err(crate::Error::Js(crate::error::JsException {
                        name: "Error".into(),
                        message: format!("getaddrinfo {code} {host}"),
                        stack: None,
                        code: Some(code.into()),
                    }))
                }
            }
        },
        |list, ctx| {
            let mut items = Vec::new();
            for (address, family) in list {
                items.push(object(ctx, vec![("address", js_str(&address)), ("family", JsValue::from(family))])?);
            }
            Ok(super::array(ctx, items))
        },
    ))
}

pub(crate) const DNS_OPS: &[(&str, usize, OpFn)] = &[("dnsLookup", 1, op_dns_lookup)];
