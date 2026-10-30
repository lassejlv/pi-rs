//! Byte channels over UNIX domain sockets: the IPC channel of `child_process.fork()` (both
//! ends: the parent's `subprocess.send()` and the child's `process.send()`, Node's
//! `NODE_CHANNEL_FD` protocol) and extra `stdio` pipes of spawned children.
//!
//! Each channel has a reader task posting `internal.ipcEvent(id, "data", bytes)` /
//! `internal.ipcEvent(id, "close")` to the JS thread and a writer task fed by `ipcWrite`.
//! Messages may carry file descriptors (`SCM_RIGHTS`, used to pass server and socket handles
//! between cluster processes): a received descriptor arrives as
//! `internal.ipcEvent(id, "fd", fd)` before the data it was sent with.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

use crate::js::{Context, JsArgs, JsResult, JsValue, js_string};
use tokio::io::Interest;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use super::{bytes_arg, bytes_to_js};
use crate::engine::{self, OpFn, arg_f64, js_str, state};

enum Out {
    Data(Vec<u8>, Option<OwnedFd>),
    Close,
}

struct Channel {
    tx: mpsc::UnboundedSender<Out>,
    fd: RawFd,
    /// Writes queued for the writer task and not yet written.
    pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

thread_local! {
    static CHANNELS: RefCell<HashMap<u64, Channel>> = RefCell::new(HashMap::new());
}

fn dispatch(ctx: &mut Context, id: u64, args: Vec<JsValue>) {
    let Some(internal) = engine::try_internal(ctx) else { return };
    let Ok(f) = internal.get(js_string!("ipcEvent"), ctx) else { return };
    let Some(f) = f.as_function() else { return };
    let mut all = vec![JsValue::from(id as f64)];
    all.extend(args);
    if let Err(e) = f.call(&JsValue::undefined(), &all, ctx) {
        engine::report_uncaught(ctx, e);
    }
}

/// `sendmsg` with an optional descriptor in an `SCM_RIGHTS` control message.
fn send_with_fd(sock: RawFd, data: &[u8], fd: Option<RawFd>) -> std::io::Result<usize> {
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: data.len() };
    // SAFETY: msghdr is plain data; the control buffer outlives the call and is sized with
    // CMSG_SPACE for one descriptor.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut control = vec![0u8; space];
        if let Some(fd) = fd {
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = space as _;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
        }
        let n = libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL);
        if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
    }
}

/// `recvmsg` returning the bytes read and any descriptors received with them.
fn recv_with_fds(sock: RawFd, buf: &mut [u8]) -> std::io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    // SAFETY: as in `send_with_fd`; received descriptors are taken into `OwnedFd`s.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        let space = libc::CMSG_SPACE((std::mem::size_of::<RawFd>() * 8) as u32) as usize;
        let mut control = vec![0u8; space];
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let flags = 0;
        let n = libc::recvmsg(sock, &mut msg, flags);
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut fds = Vec::new();
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg);
                let len = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for i in 0..len / std::mem::size_of::<RawFd>() {
                    let fd = std::ptr::read_unaligned(data.cast::<RawFd>().add(i));
                    let fd = OwnedFd::from_raw_fd(fd);
                    #[cfg(not(any(target_os = "linux", target_os = "android")))]
                    if libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    fds.push(fd);
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        Ok((n as usize, fds))
    }
}

/// Register a connected socket as a channel and start its tasks. Returns the channel id.
pub(crate) fn start(ctx: &mut Context, stream: std::os::unix::net::UnixStream) -> std::io::Result<u64> {
    let st = state(ctx);
    stream.set_nonblocking(true)?;
    let stream = {
        let _guard = st.tokio.enter();
        UnixStream::from_std(stream)?
    };
    let stream = std::sync::Arc::new(stream);
    let id = st.next_id();
    let poster = st.poster();
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    let pending = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    CHANNELS.with(|c| c.borrow_mut().insert(id, Channel { tx, fd: stream.as_raw_fd(), pending: pending.clone() }));
    let reader = stream.clone();
    let read_poster = poster.clone();
    st.tokio.spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let r = reader.async_io(Interest::READABLE, || recv_with_fds(reader.as_raw_fd(), &mut buf)).await;
            match r {
                Ok((0, _)) | Err(_) => break,
                Ok((n, fds)) => {
                    let chunk = buf[..n].to_vec();
                    let fds: Vec<RawFd> = fds.into_iter().map(IntoRawFd::into_raw_fd).collect();
                    read_poster.post(move |ctx| {
                        for fd in fds {
                            dispatch(ctx, id, vec![js_str("fd"), JsValue::from(fd)]);
                        }
                        if let Ok(b) = bytes_to_js(&chunk, ctx) {
                            dispatch(ctx, id, vec![js_str("data"), b]);
                        }
                    });
                }
            }
        }
        read_poster.post(move |ctx| {
            CHANNELS.with(|c| c.borrow_mut().remove(&id));
            dispatch(ctx, id, vec![js_str("close")]);
        });
    });
    st.tokio.spawn(async move {
        while let Some(out) = rx.recv().await {
            match out {
                Out::Data(data, fd) => {
                    let mut off = 0;
                    let mut fd = fd;
                    while off < data.len() {
                        let raw = fd.as_ref().map(AsRawFd::as_raw_fd);
                        let r = stream.async_io(Interest::WRITABLE, || send_with_fd(stream.as_raw_fd(), &data[off..], raw)).await;
                        match r {
                            Ok(n) => {
                                off += n;
                                fd = None;
                            }
                            Err(_) => return,
                        }
                    }
                    pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
                Out::Close => {
                    // SAFETY: shutting down our own socket.
                    unsafe { libc::shutdown(stream.as_raw_fd(), libc::SHUT_RDWR) };
                    return;
                }
            }
        }
    });
    Ok(id)
}

/// `ops.ipcOpen(fd) -> id`: adopt an inherited descriptor (the child's `NODE_CHANNEL_FD`).
fn op_ipc_open(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let fd = arg_f64(args, 0, ctx)? as RawFd;
    // SAFETY: the descriptor was inherited for this purpose and is owned from now on.
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    // SAFETY: plain fcntl on our descriptor.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    match start(ctx, stream) {
        Ok(id) => Ok(JsValue::from(id as f64)),
        Err(e) => Err(super::throw_io(ctx, &e, "open", None)),
    }
}

/// `ops.ipcWrite(id, data, fd?) -> bool`: queue bytes (with a descriptor to pass).
fn op_ipc_write(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let data = bytes_arg(args.get_or_undefined(1), ctx)?;
    let fd = args.get_or_undefined(2);
    let fd = if fd.is_null_or_undefined() {
        None
    } else {
        let raw = fd.to_number(ctx)? as RawFd;
        // SAFETY: dup of a descriptor the caller owns; the copy is closed after sending.
        let dup = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            let e = std::io::Error::last_os_error();
            return Err(super::throw_io(ctx, &e, "dup", None));
        }
        // SAFETY: `dup` is a fresh descriptor we own.
        Some(unsafe { OwnedFd::from_raw_fd(dup) })
    };
    let ok = CHANNELS.with(|c| {
        c.borrow().get(&id).is_some_and(|ch| {
            ch.pending.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let ok = ch.tx.send(Out::Data(data, fd)).is_ok();
            if !ok {
                ch.pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
            ok
        })
    });
    Ok(JsValue::from(ok))
}

/// `ops.ipcClose(id)`
fn op_ipc_close(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    CHANNELS.with(|c| {
        if let Some(ch) = c.borrow_mut().remove(&id) {
            // Nothing left to write: shut down now, so the peer sees the disconnect before
            // anything this process does next (e.g. exiting).
            if ch.pending.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                // SAFETY: shutting down our own socket (the writer task owns the descriptor).
                unsafe { libc::shutdown(ch.fd, libc::SHUT_RDWR) };
            }
            let _ = ch.tx.send(Out::Close);
        }
    });
    Ok(JsValue::undefined())
}

/// `ops.closeFd(fd)`: close a descriptor received over a channel that was not adopted.
fn op_close_fd(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let fd = arg_f64(args, 0, ctx)? as RawFd;
    // SAFETY: closing a descriptor this runtime owns.
    unsafe { libc::close(fd) };
    Ok(JsValue::undefined())
}

pub(crate) fn teardown(_ctx: &mut Context) {
    CHANNELS.with(|c| {
        for (_, ch) in c.borrow_mut().drain() {
            let _ = ch.tx.send(Out::Close);
        }
    });
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("ipcOpen", 1, op_ipc_open),
    ("ipcWrite", 3, op_ipc_write),
    ("ipcClose", 1, op_ipc_close),
    ("closeFd", 1, op_close_fd),
];
