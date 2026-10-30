//! UDP sockets backing `node:dgram` (`js/node/dgram.js`) on tokio, with socket options set
//! through socket2. Received datagrams are posted to the JS thread and dispatched through
//! `internal.dgramEvent(id, kind, ...)`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use crate::js::{Context, JsArgs, JsResult, JsValue, js_string};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use super::{bytes_arg, bytes_to_js, io_code, object};
use crate::engine::{self, OpFn, arg_f64, arg_string, js_str, state};

struct Udp {
    ipv6: bool,
    /// Created at bind time (socket2), shared with the reader task.
    sock: Option<Arc<UdpSocket>>,
    raw: Option<Socket>,
    stop: CancellationToken,
}

thread_local! {
    static SOCKETS: RefCell<HashMap<u64, Udp>> = RefCell::new(HashMap::new());
}

fn dispatch(ctx: &mut Context, id: u64, args: Vec<JsValue>) {
    let Some(internal) = engine::try_internal(ctx) else { return };
    let Ok(f) = internal.get(js_string!("dgramEvent"), ctx) else { return };
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

fn err_obj(ctx: &mut Context, e: &std::io::Error, syscall: &str, address: Option<(&str, u16)>) -> JsResult<JsValue> {
    let code = io_code(e);
    let code = if e.raw_os_error() == Some(libc::EADDRINUSE) {
        "EADDRINUSE"
    } else if e.raw_os_error() == Some(libc::EADDRNOTAVAIL) {
        "EADDRNOTAVAIL"
    } else if e.raw_os_error() == Some(libc::ENOTCONN) {
        "ENOTCONN"
    } else {
        code
    };
    let mut message = format!("{syscall} {code}");
    let mut entries = vec![
        ("errno", JsValue::from(-e.raw_os_error().unwrap_or(0))),
        ("code", js_str(code)),
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

fn resolve(host: &str, port: u16, ipv6: bool) -> std::io::Result<SocketAddr> {
    let addrs = (host, port).to_socket_addrs()?;
    let mut first = None;
    for a in addrs {
        if a.is_ipv6() == ipv6 {
            return Ok(a);
        }
        first.get_or_insert(a);
    }
    first.ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))
}

/// `ops.dgramCreate(ipv6) -> id`
fn op_dgram_create(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ipv6 = args.get_or_undefined(0).to_boolean();
    let st = state(ctx);
    let id = st.next_id();
    let domain = if ipv6 { Domain::IPV6 } else { Domain::IPV4 };
    let raw = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).map_err(|e| super::throw_io(ctx, &e, "socket", None))?;
    let _ = raw.set_nonblocking(true);
    let _ = raw.set_cloexec(true);
    SOCKETS.with(|s| s.borrow_mut().insert(id, Udp { ipv6, sock: None, raw: Some(raw), stop: CancellationToken::new() }));
    Ok(JsValue::from(id as f64))
}

fn start_reader(ctx: &mut Context, id: u64, sock: Arc<UdpSocket>, stop: CancellationToken) {
    let st = state(ctx);
    let poster = st.poster();
    st.tokio.spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            let r = tokio::select! {
                _ = stop.cancelled() => return,
                r = sock.recv_from(&mut buf) => r,
            };
            match r {
                Ok((n, from)) => {
                    let data = buf[..n].to_vec();
                    poster.post(move |ctx| {
                        let Ok(bytes) = bytes_to_js(&data, ctx) else { return };
                        let Ok(rinfo) = object(
                            ctx,
                            vec![
                                ("address", js_str(&from.ip().to_string())),
                                ("family", js_str(family(&from))),
                                ("port", JsValue::from(from.port())),
                                ("size", JsValue::from(n as u32)),
                            ],
                        ) else {
                            return;
                        };
                        dispatch(ctx, id, vec![js_str("message"), bytes, rinfo]);
                    });
                }
                Err(e) => {
                    // ICMP port unreachable etc. on connected sockets: report and keep going.
                    poster.post(move |ctx| {
                        if let Ok(err) = err_obj(ctx, &e, "recvmsg", None) {
                            dispatch(ctx, id, vec![js_str("error"), err]);
                        }
                    });
                    if stop.is_cancelled() {
                        return;
                    }
                }
            }
        }
    });
}

/// `ops.dgramBind(id, address, port, { reuseAddr, reusePort, ipv6Only, exclusive }) ->
/// { address, family, port } | { error }`
fn op_dgram_bind(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let host = arg_string(args, 1, ctx)?;
    let port = arg_f64(args, 2, ctx)? as u16;
    let opts = args.get_or_undefined(3).as_object();
    let flag = |name: &str, ctx: &mut Context| -> bool {
        opts.as_ref().and_then(|o| o.get(js_str(name).as_string().unwrap().clone(), ctx).ok()).is_some_and(|v| v.to_boolean())
    };
    let reuse_addr = flag("reuseAddr", ctx);
    let reuse_port = flag("reusePort", ctx);
    let ipv6_only = flag("ipv6Only", ctx);
    let taken = SOCKETS.with(|s| s.borrow_mut().get_mut(&id).map(|u| (u.raw.take(), u.ipv6, u.stop.clone())));
    let Some((Some(raw), ipv6, stop)) = taken else {
        return object(ctx, vec![("error", js_str("not running"))]);
    };
    let addr = if host.is_empty() {
        Ok(if ipv6 { SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port) } else { SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port) })
    } else {
        resolve(&host, port, ipv6)
    };
    let result = addr.and_then(|addr| {
        if reuse_addr {
            raw.set_reuse_address(true)?;
        }
        if reuse_port {
            raw.set_reuse_port(true)?;
        }
        if ipv6 && ipv6_only {
            raw.set_only_v6(true)?;
        }
        raw.bind(&addr.into())?;
        Ok(addr)
    });
    match result {
        Ok(_) => {
            let std_sock: std::net::UdpSocket = raw.into();
            let local = std_sock.local_addr().ok();
            let sock = {
                let _g = state(ctx).tokio.enter();
                UdpSocket::from_std(std_sock).map_err(|e| super::throw_io(ctx, &e, "bind", None))?
            };
            let sock = Arc::new(sock);
            SOCKETS.with(|s| {
                if let Some(u) = s.borrow_mut().get_mut(&id) {
                    u.sock = Some(sock.clone());
                }
            });
            start_reader(ctx, id, sock, stop);
            let local = local.unwrap_or_else(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
            object(
                ctx,
                vec![
                    ("address", js_str(&local.ip().to_string())),
                    ("family", js_str(family(&local))),
                    ("port", JsValue::from(local.port())),
                ],
            )
        }
        Err(e) => {
            // The socket stays usable for another bind attempt.
            let err = err_obj(ctx, &e, "bind", Some((&host, port)))?;
            object(ctx, vec![("error", err)])
        }
    }
}

fn with_sock<R>(id: u64, f: impl FnOnce(&Arc<UdpSocket>) -> R) -> Option<R> {
    SOCKETS.with(|s| s.borrow().get(&id).and_then(|u| u.sock.as_ref().map(f)))
}

/// `ops.dgramSend(id, bytes, address | null, port, callbackId) -> bytes | error | undefined`: the
/// result of an immediate send, or `undefined` when it completes later with a "sent" event
/// (cbId, error | bytes).
fn op_dgram_send(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let data = bytes_arg(args.get_or_undefined(1), ctx)?;
    let host = args.get_or_undefined(2);
    let host = if host.is_null_or_undefined() { None } else { Some(host.to_string(ctx)?.to_std_string_lossy()) };
    let port = args.get_or_undefined(3).to_number(ctx).unwrap_or(0.0) as u16;
    let cb = arg_f64(args, 4, ctx)?;
    let ipv6 = SOCKETS.with(|s| s.borrow().get(&id).map(|u| u.ipv6)).unwrap_or(false);
    let Some(sock) = with_sock(id, Arc::clone) else { return Ok(JsValue::from(false)) };
    // Like libuv's uv_udp_try_send: send now when the socket is writable (IP literals need no
    // lookup); the result is returned and JS calls back on the next tick.
    let literal = host.as_deref().map(|h| h.parse::<IpAddr>().map(|ip| SocketAddr::new(ip, port)));
    let immediate = match literal {
        None => Some(sock.try_send(&data)),
        Some(Ok(addr)) => Some(sock.try_send_to(&data, addr)),
        Some(Err(_)) => None,
    };
    match immediate {
        Some(Ok(n)) => return Ok(JsValue::from(n as u32)),
        Some(Err(e)) if e.kind() != std::io::ErrorKind::WouldBlock => {
            return err_obj(ctx, &e, "send", host.as_deref().map(|h| (h, port)));
        }
        _ => {}
    }
    let st = state(ctx);
    let poster = st.poster();
    st.tokio.spawn(async move {
        let r = match &host {
            Some(h) => {
                let h2 = h.clone();
                let addr = tokio::task::spawn_blocking(move || resolve(&h2, port, ipv6)).await.unwrap_or_else(|_| Err(std::io::Error::other("lookup failed")));
                match addr {
                    Ok(a) => sock.send_to(&data, a).await.map_err(|e| (e, "send")),
                    Err(e) => Err((e, "getaddrinfo")),
                }
            }
            None => sock.send(&data).await.map_err(|e| (e, "send")),
        };
        poster.post(move |ctx| {
            let v = match r {
                Ok(n) => JsValue::from(n as u32),
                Err((e, syscall)) => err_obj(ctx, &e, syscall, host.as_deref().map(|h| (h, port))).unwrap_or_default(),
            };
            dispatch(ctx, id, vec![js_str("sent"), JsValue::from(cb), v]);
        });
    });
    Ok(JsValue::undefined())
}

/// `ops.dgramConnect(id, address, port)`: posts "connect" (error | undefined).
fn op_dgram_connect(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let host = arg_string(args, 1, ctx)?;
    let port = arg_f64(args, 2, ctx)? as u16;
    let ipv6 = SOCKETS.with(|s| s.borrow().get(&id).map(|u| u.ipv6)).unwrap_or(false);
    let Some(sock) = with_sock(id, Arc::clone) else { return Ok(JsValue::from(false)) };
    let st = state(ctx);
    let poster = st.poster();
    st.tokio.spawn(async move {
        let h2 = host.clone();
        let addr = tokio::task::spawn_blocking(move || resolve(&h2, port, ipv6)).await.unwrap_or_else(|_| Err(std::io::Error::other("lookup failed")));
        let r = match addr {
            Ok(a) => sock.connect(a).await.map_err(|e| (e, "connect")),
            Err(e) => Err((e, "getaddrinfo")),
        };
        poster.post(move |ctx| {
            let v = match r {
                Ok(()) => JsValue::undefined(),
                Err((e, syscall)) => err_obj(ctx, &e, syscall, Some((&host, port))).unwrap_or_default(),
            };
            dispatch(ctx, id, vec![js_str("connect"), v]);
        });
    });
    Ok(JsValue::from(true))
}

/// `ops.dgramDisconnect(id)`: connect to AF_UNSPEC.
fn op_dgram_disconnect(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    with_sock(id, |s| {
        use std::os::fd::AsRawFd;
        // SAFETY: AF_UNSPEC sockaddr dissolves the association (connect(2)).
        unsafe {
            let mut addr: libc::sockaddr = std::mem::zeroed();
            addr.sa_family = libc::AF_UNSPEC as libc::sa_family_t;
            libc::connect(s.as_raw_fd(), &addr, std::mem::size_of::<libc::sockaddr>() as libc::socklen_t);
        }
    });
    Ok(JsValue::undefined())
}

/// `ops.dgramAddress(id, remote) -> { address, family, port } | { error }`
fn op_dgram_address(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let remote = args.get_or_undefined(1).to_boolean();
    let r = with_sock(id, |s| if remote { s.peer_addr() } else { s.local_addr() });
    match r {
        Some(Ok(a)) => object(ctx, vec![("address", js_str(&a.ip().to_string())), ("family", js_str(family(&a))), ("port", JsValue::from(a.port()))]),
        Some(Err(e)) => {
            let err = err_obj(ctx, &e, if remote { "getpeername" } else { "getsockname" }, None)?;
            object(ctx, vec![("error", err)])
        }
        None => {
            let e = std::io::Error::from_raw_os_error(libc::EBADF);
            let err = err_obj(ctx, &e, if remote { "getpeername" } else { "getsockname" }, None)?;
            object(ctx, vec![("error", err)])
        }
    }
}

/// `ops.dgramOption(id, name, a, b) -> result | { error }`: socket options.
fn op_dgram_option(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let name = arg_string(args, 1, ctx)?;
    let a = args.get_or_undefined(2).clone();
    let b = args.get_or_undefined(3).clone();
    let a_num = a.to_number(ctx).unwrap_or(0.0);
    let a_str = a.to_string(ctx).map(|s| s.to_std_string_lossy()).unwrap_or_default();
    let b_str = if b.is_null_or_undefined() { None } else { Some(b.to_string(ctx)?.to_std_string_lossy()) };
    let r: Option<std::io::Result<f64>> = with_sock(id, |s| {
        let sock = socket2::SockRef::from(s.as_ref());
        let v6 = s.local_addr().is_ok_and(|a| a.is_ipv6());
        match name.as_str() {
            "broadcast" => s.set_broadcast(a.to_boolean()).map(|_| 0.0),
            "ttl" => s.set_ttl(a_num as u32).map(|_| a_num),
            "multicastTTL" => if v6 { sock.set_multicast_hops_v6(a_num as u32) } else { s.set_multicast_ttl_v4(a_num as u32) }.map(|_| a_num),
            "multicastLoopback" => if v6 { s.set_multicast_loop_v6(a.to_boolean()) } else { s.set_multicast_loop_v4(a.to_boolean()) }.map(|_| if a.to_boolean() { 1.0 } else { 0.0 }),
            "addMembership" | "dropMembership" => {
                let group: Result<IpAddr, _> = a_str.parse();
                match group {
                    Ok(IpAddr::V4(g)) => {
                        let iface = b_str.as_deref().and_then(|i| i.parse::<Ipv4Addr>().ok()).unwrap_or(Ipv4Addr::UNSPECIFIED);
                        if name == "addMembership" { s.join_multicast_v4(g, iface) } else { s.leave_multicast_v4(g, iface) }.map(|_| 0.0)
                    }
                    Ok(IpAddr::V6(g)) => if name == "addMembership" { s.join_multicast_v6(&g, 0) } else { s.leave_multicast_v6(&g, 0) }.map(|_| 0.0),
                    Err(_) => Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
                }
            }
            "multicastInterface" => match a_str.parse::<Ipv4Addr>() {
                Ok(i) => sock.set_multicast_if_v4(&i).map(|_| 0.0),
                Err(_) => Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
            },
            "recvBufferSize" => sock.set_recv_buffer_size(a_num as usize).map(|_| 0.0),
            "sendBufferSize" => sock.set_send_buffer_size(a_num as usize).map(|_| 0.0),
            "getRecvBufferSize" => sock.recv_buffer_size().map(|n| n as f64),
            "getSendBufferSize" => sock.send_buffer_size().map(|n| n as f64),
            _ => Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
        }
    });
    let syscall = match name.as_str() {
        "broadcast" => "setBroadcast",
        "ttl" => "setTTL",
        "multicastTTL" => "setMulticastTTL",
        "multicastLoopback" => "setMulticastLoopback",
        "addMembership" => "addMembership",
        "dropMembership" => "dropMembership",
        "multicastInterface" => "setMulticastInterface",
        "recvBufferSize" | "getRecvBufferSize" => "uv_recv_buffer_size",
        _ => "uv_send_buffer_size",
    };
    match r {
        Some(Ok(v)) => Ok(JsValue::from(v)),
        Some(Err(e)) => {
            let err = err_obj(ctx, &e, syscall, None)?;
            object(ctx, vec![("error", err)])
        }
        None => {
            let e = std::io::Error::from_raw_os_error(libc::EBADF);
            let err = err_obj(ctx, &e, syscall, None)?;
            object(ctx, vec![("error", err)])
        }
    }
}

/// `ops.dgramClose(id)`
fn op_dgram_close(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    if let Some(u) = SOCKETS.with(|s| s.borrow_mut().remove(&id)) {
        u.stop.cancel();
    }
    Ok(JsValue::undefined())
}

pub(crate) fn teardown(_ctx: &mut Context) {
    SOCKETS.with(|s| s.borrow_mut().drain().for_each(|(_, u)| u.stop.cancel()));
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("dgramCreate", 1, op_dgram_create),
    ("dgramBind", 4, op_dgram_bind),
    ("dgramSend", 5, op_dgram_send),
    ("dgramConnect", 3, op_dgram_connect),
    ("dgramDisconnect", 1, op_dgram_disconnect),
    ("dgramAddress", 2, op_dgram_address),
    ("dgramOption", 4, op_dgram_option),
    ("dgramClose", 1, op_dgram_close),
];
