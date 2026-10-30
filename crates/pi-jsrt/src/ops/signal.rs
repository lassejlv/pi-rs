//! OS signals delivered to `process.on('SIGINT' | 'SIGTERM' | ...)` listeners.
//!
//! A signal gets a process-wide handler the first time any runtime listens for it. The handler
//! cooperates with the embedder's handlers like Node's libuv does for an embedding process:
//!
//! - while a JS listener exists, the signal is queued for the JS thread(s) and the handler that
//!   was installed before ours (pi's own, e.g. tokio's SIGTERM/SIGHUP handling) still runs; the
//!   default action (terminate) is suppressed, as in Node;
//! - without listeners it behaves as if it were not installed: a previous handler runs, an
//!   ignored signal stays ignored and a default action is performed (the signal is re-raised
//!   with the default disposition).
//!
//! The handler only writes the signal number to a pipe (async-signal-safe); a dispatcher thread
//! posts `internal.signalEvent(signo)` to every runtime listening for it.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::Once;

use boa_engine::{Context, JsResult, JsValue, js_string};
use parking_lot::Mutex;

use super::coded_error;
use crate::engine::{self, OpFn, Poster, arg_f64, js_str, state};

const NSIG: usize = 65;

struct OldActions(UnsafeCell<[libc::sigaction; NSIG]>);
// SAFETY: an entry is written once, before its `INSTALLED` flag is set (Release); the signal
// handler reads it only after observing the flag (Acquire).
unsafe impl Sync for OldActions {}

// SAFETY: an all-zero sigaction is a valid value.
static OLD: OldActions = OldActions(UnsafeCell::new(unsafe { std::mem::zeroed() }));
static INSTALLED: [AtomicBool; NSIG] = [const { AtomicBool::new(false) }; NSIG];
static ACTIVE: [AtomicUsize; NSIG] = [const { AtomicUsize::new(0) }; NSIG];
static PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);
/// Signals written to the pipe and not yet handed to the runtimes: a runtime whose event loop
/// would end now stays alive until they are delivered (a signal raised just before).
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn in_flight() -> bool {
    IN_FLIGHT.load(Ordering::SeqCst) > 0
}
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

struct Subscriber {
    signo: i32,
    runtime: usize,
    poster: Poster,
}

static SUBSCRIBERS: Mutex<Vec<Subscriber>> = Mutex::new(Vec::new());

/// Signals whose default action is to ignore them.
fn default_ignored(sig: i32) -> bool {
    matches!(sig, libc::SIGCHLD | libc::SIGWINCH | libc::SIGURG | libc::SIGCONT)
}

extern "C" fn handler(sig: libc::c_int, info: *mut libc::siginfo_t, uctx: *mut libc::c_void) {
    let idx = sig as usize;
    if idx >= NSIG || !INSTALLED[idx].load(Ordering::Acquire) {
        return;
    }
    // SAFETY: written before INSTALLED was set; never written again.
    let old = unsafe { &(*OLD.0.get())[idx] };
    let listening = ACTIVE[idx].load(Ordering::SeqCst) > 0;
    if listening {
        let fd = PIPE_WRITE.load(Ordering::Relaxed);
        if fd >= 0 {
            let b = [sig as u8];
            IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
            // SAFETY: write(2) is async-signal-safe.
            if unsafe { libc::write(fd, b.as_ptr().cast(), 1) } != 1 {
                IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
    match old.sa_sigaction {
        libc::SIG_DFL => {
            if listening || default_ignored(sig) {
                return;
            }
            // No JS listener: perform the default action. Restore it and re-raise; the signal
            // is delivered when this handler returns. Stop signals continue afterwards, so the
            // handler is reinstalled by the next subscription.
            INSTALLED[idx].store(false, Ordering::Release);
            // SAFETY: sigaction/raise are async-signal-safe.
            unsafe {
                libc::sigaction(sig, old, std::ptr::null_mut());
                libc::raise(sig);
            }
        }
        libc::SIG_IGN => {}
        f => {
            // SAFETY: chaining to the handler that was installed before ours, with the calling
            // convention its flags declare.
            unsafe {
                if old.sa_flags & libc::SA_SIGINFO != 0 {
                    let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) = std::mem::transmute(f);
                    f(sig, info, uctx);
                } else {
                    let f: extern "C" fn(libc::c_int) = std::mem::transmute(f);
                    f(sig);
                }
            }
        }
    }
}

/// Start the dispatcher thread and its pipe (once).
fn start_dispatcher() -> bool {
    static START: Once = Once::new();
    START.call_once(|| {
        let mut fds = [0i32; 2];
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: pipe2 fills the array on success and atomically sets close-on-exec.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return;
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        // SAFETY: pipe fills the array on success. Unlike pipe2, it is available on macOS.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return;
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        for fd in fds {
            // SAFETY: these are the descriptors just created above.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return;
            }
        }
        let (r, w) = (fds[0], fds[1]);
        // SAFETY: plain fcntl on our own descriptor; a full pipe drops signals instead of blocking
        // the handler (signals coalesce anyway).
        unsafe { libc::fcntl(w, libc::F_SETFL, libc::fcntl(w, libc::F_GETFL) | libc::O_NONBLOCK) };
        PIPE_WRITE.store(w, Ordering::SeqCst);
        let _ = std::thread::Builder::new().name("pi-jsrt-signals".into()).spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                // SAFETY: reading into our buffer from our pipe.
                let n = unsafe { libc::read(r, buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return;
                }
                if n == 0 {
                    return;
                }
                let subs = SUBSCRIBERS.lock();
                for &sig in &buf[..n as usize] {
                    let sig = sig as i32;
                    for s in subs.iter().filter(|s| s.signo == sig) {
                        s.poster.post(move |ctx| dispatch(ctx, sig));
                    }
                    // Posted: each runtime's queue now keeps it alive until it runs.
                    IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                }
            }
        });
    });
    PIPE_WRITE.load(Ordering::SeqCst) >= 0
}

fn dispatch(ctx: &mut Context, sig: i32) {
    let Some(internal) = engine::try_internal(ctx) else {
        return;
    };
    let Ok(f) = internal.get(js_string!("signalEvent"), ctx) else {
        return;
    };
    let Some(f) = f.as_function() else { return };
    if let Err(e) = f.call(&JsValue::undefined(), &[JsValue::from(sig)], ctx) {
        engine::report_uncaught(ctx, e);
    }
}

fn install(sig: i32) -> Result<(), i32> {
    let idx = sig as usize;
    if idx == 0 || idx >= NSIG || sig == libc::SIGKILL || sig == libc::SIGSTOP {
        return Err(libc::EINVAL);
    }
    let _guard = INSTALL_LOCK.lock();
    if INSTALLED[idx].load(Ordering::Acquire) {
        return Ok(());
    }
    // SAFETY: sigaction with a zeroed, then filled, struct; OLD[idx] is written before the flag.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART | libc::SA_ONSTACK;
        libc::sigemptyset(&mut action.sa_mask);
        let old = &mut (*OLD.0.get())[idx];
        if libc::sigaction(sig, std::ptr::null(), old) != 0 {
            return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL));
        }
        INSTALLED[idx].store(true, Ordering::Release);
        if libc::sigaction(sig, &action, std::ptr::null_mut()) != 0 {
            INSTALLED[idx].store(false, Ordering::Release);
            return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL));
        }
    }
    Ok(())
}

fn runtime_key(ctx: &Context) -> usize {
    std::sync::Arc::as_ptr(&state(ctx).shared) as usize
}

/// `ops.signalSubscribe(signo)`: deliver `signo` to this runtime (`uv_signal_start`).
fn op_signal_subscribe(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let sig = arg_f64(args, 0, ctx)? as i32;
    let failed = |ctx: &mut Context, errno: i32| {
        let code = if errno == libc::EINVAL { "EINVAL" } else { "EIO" };
        coded_error(
            ctx,
            "Error",
            &format!("uv_signal_start {code}"),
            &[("errno", JsValue::from(-errno)), ("code", js_str(code)), ("syscall", js_str("uv_signal_start"))],
        )
    };
    if !start_dispatcher() {
        return Err(failed(ctx, libc::EIO));
    }
    if let Err(errno) = install(sig) {
        return Err(failed(ctx, errno));
    }
    SUBSCRIBERS.lock().push(Subscriber { signo: sig, runtime: runtime_key(ctx), poster: state(ctx).poster() });
    ACTIVE[sig as usize].fetch_add(1, Ordering::SeqCst);
    Ok(JsValue::undefined())
}

/// `ops.signalUnsubscribe(signo)`
fn op_signal_unsubscribe(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let sig = arg_f64(args, 0, ctx)? as i32;
    let me = runtime_key(ctx);
    let mut subs = SUBSCRIBERS.lock();
    if let Some(i) = subs.iter().position(|s| s.signo == sig && s.runtime == me) {
        subs.remove(i);
        ACTIVE[sig as usize].fetch_sub(1, Ordering::SeqCst);
    }
    Ok(JsValue::undefined())
}

pub(crate) fn teardown(ctx: &mut Context) {
    let me = runtime_key(ctx);
    SUBSCRIBERS.lock().retain(|s| {
        if s.runtime == me {
            ACTIVE[s.signo as usize].fetch_sub(1, Ordering::SeqCst);
            false
        } else {
            true
        }
    });
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[("signalSubscribe", 1, op_signal_subscribe), ("signalUnsubscribe", 1, op_signal_unsubscribe)];
