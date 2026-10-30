//! `process` / `os` ops and the static configuration object.

use crate::js::{Context, JsArgs, JsObject, JsResult, JsString, JsValue, js_string};

use super::{array, object};
use crate::engine::{OpFn, arg_f64, arg_opt_string, js_str, state};

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buffer is valid for its length; gethostname NUL-terminates on success.
    let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if r != 0 {
        return "localhost".into();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

struct Uname {
    sysname: String,
    release: String,
    version: String,
    machine: String,
}

fn uname() -> Uname {
    // SAFETY: utsname is plain data; uname fills it.
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::uname(&mut u) } == 0;
    let f = |s: &[libc::c_char]| {
        let bytes: Vec<u8> = s.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    if !ok {
        return Uname {
            sysname: "Linux".into(),
            release: String::new(),
            version: String::new(),
            machine: std::env::consts::ARCH.into(),
        };
    }
    Uname {
        sysname: f(&u.sysname),
        release: f(&u.release),
        version: f(&u.version),
        machine: f(&u.machine),
    }
}

fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

/// Static configuration passed to the glue as `ops.config`.
pub(crate) fn config_object(ctx: &mut Context) -> JsResult<JsValue> {
    let st = state(ctx);
    let shared = st.shared.clone();
    let env = JsObject::with_object_proto(ctx.intrinsics());
    let mut keys: Vec<_> = if shared.options.node_compat { shared.env.iter().collect() } else { Vec::new() };
    keys.sort();
    for (k, v) in keys {
        env.set(JsString::from(k.as_str()), js_str(v), true, ctx)?;
    }
    let argv: Vec<JsValue> = shared.options.argv.iter().map(|a| js_str(a)).collect();
    let argv = array(ctx, argv);
    let exec_argv: Vec<JsValue> = shared.options.exec_argv.iter().map(|a| js_str(a)).collect();
    let exec_argv = array(ctx, exec_argv);
    let node_exec: Vec<JsValue> = match &shared.options.node_exec {
        Some(v) => v.iter().map(|a| js_str(a)).collect(),
        None => vec![
            js_str(&std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()),
            js_str(crate::node::NODE_MARKER),
        ],
    };
    let node_exec = array(ctx, node_exec);
    let exec_path = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    let home = dirs::home_dir().map(|p| p.display().to_string()).unwrap_or_else(|| "/".into());
    let tmp = std::env::temp_dir().display().to_string();
    let tmp = if tmp.len() > 1 { tmp.trim_end_matches('/').to_string() } else { tmp };
    let u = uname();
    object(
        ctx,
        vec![
            ("nodeCompat", JsValue::from(shared.options.node_compat)),
            ("env", env.into()),
            ("argv", argv),
            ("execArgv", exec_argv),
            ("nodeExec", node_exec),
            ("cwd", js_str(&shared.cwd.display().to_string())),
            ("platform", js_str(node_platform())),
            ("arch", js_str(node_arch())),
            ("pid", JsValue::from(std::process::id())),
            // SAFETY: getppid has no preconditions.
            ("ppid", JsValue::from(unsafe { libc::getppid() })),
            ("execPath", js_str(&exec_path)),
            ("hostname", js_str(&hostname())),
            ("homedir", js_str(&home)),
            ("tmpdir", js_str(&tmp)),
            ("osType", js_str(&u.sysname)),
            ("osRelease", js_str(&u.release)),
            ("osVersion", js_str(&u.version)),
            ("machine", js_str(&u.machine)),
            ("version", js_str(env!("CARGO_PKG_VERSION"))),
            ("hasExitHandler", JsValue::from(shared.options.on_exit.is_some())),
            ("stdin", JsValue::from(shared.options.stdin)),
        ],
    )
}

fn op_exit(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let code = args.get_or_undefined(0).to_number(ctx).unwrap_or(0.0) as i32;
    let st = state(ctx);
    if let Some(h) = &st.shared.options.on_exit {
        h(code);
    }
    Ok(JsValue::undefined())
}

fn op_kill(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let pid = arg_f64(args, 0, ctx)? as i32;
    let sig = args.get_or_undefined(1).to_number(ctx).unwrap_or(15.0) as i32;
    // SAFETY: plain syscall.
    let r = unsafe { libc::kill(pid, sig) };
    if r != 0 {
        // Node's uvException: "kill ESRCH" with errno/code/syscall.
        let e = std::io::Error::last_os_error();
        let code = super::io_code(&e);
        return Err(super::coded_error(
            ctx,
            "Error",
            &format!("kill {code}"),
            &[("errno", JsValue::from(-e.raw_os_error().unwrap_or(0))), ("code", js_str(code)), ("syscall", js_str("kill"))],
        ));
    }
    Ok(JsValue::from(true))
}

fn op_isatty(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let fd = arg_f64(args, 0, ctx)? as i32;
    // SAFETY: plain syscall.
    Ok(JsValue::from(unsafe { libc::isatty(fd) } == 1))
}

fn op_term_size(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let fd = arg_f64(args, 0, ctx)? as i32;
    // SAFETY: winsize is plain data; ioctl fills it.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if r != 0 || ws.ws_col == 0 {
        return Ok(JsValue::null());
    }
    let cols = JsValue::from(ws.ws_col as u32);
    let rows = JsValue::from(ws.ws_row as u32);
    Ok(array(ctx, vec![cols, rows]))
}

fn op_memory(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let rss = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok()))
        .map(|pages| pages * 4096.0)
        .unwrap_or(0.0);
    object(
        ctx,
        vec![
            ("rss", JsValue::from(rss)),
            ("heapTotal", JsValue::from(0)),
            ("heapUsed", JsValue::from(0)),
            ("external", JsValue::from(0)),
            ("arrayBuffers", JsValue::from(0)),
        ],
    )
}

fn meminfo(key: &str) -> f64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok()))
        })
        .map(|kb| kb * 1024.0)
        .unwrap_or(0.0)
}

fn op_os_info(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let model = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1).map(|v| v.trim().to_string()))
        })
        .unwrap_or_else(|| "unknown".into());
    let uptime = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()))
        .unwrap_or(0.0);
    let load = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .map(|s| s.split_whitespace().take(3).filter_map(|v| v.parse::<f64>().ok()).collect::<Vec<_>>())
        .unwrap_or_default();
    let load: Vec<JsValue> = (0..3).map(|i| JsValue::from(load.get(i).copied().unwrap_or(0.0))).collect();
    let load = array(ctx, load);
    // SAFETY: plain syscalls.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let user = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_default();
    let shell = std::env::var("SHELL").unwrap_or_default();
    object(
        ctx,
        vec![
            ("cpus", JsValue::from(cpus as u32)),
            ("cpuModel", js_str(&model)),
            ("uptime", JsValue::from(uptime)),
            ("loadavg", load),
            ("totalmem", JsValue::from(meminfo("MemTotal:"))),
            ("freemem", JsValue::from(meminfo("MemAvailable:"))),
            ("uid", JsValue::from(uid)),
            ("gid", JsValue::from(gid)),
            ("username", js_str(&user)),
            ("shell", js_str(&shell)),
        ],
    )
}

fn op_cpu_usage(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // SAFETY: rusage is plain data.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let user = ru.ru_utime.tv_sec as f64 * 1e6 + ru.ru_utime.tv_usec as f64;
    let system = ru.ru_stime.tv_sec as f64 * 1e6 + ru.ru_stime.tv_usec as f64;
    object(ctx, vec![("user", JsValue::from(user)), ("system", JsValue::from(system))])
}

/// `ops.setEnv(name, value | undefined)`: `process.env` reports every assignment and deletion.
/// `TZ` sets the runtime's timezone, as Node's does; deleting it returns to the host's.
fn op_set_env(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_opt_string(args, 0, ctx)?;
    let value = arg_opt_string(args, 1, ctx)?;
    if name.as_deref() == Some("TZ") {
        *crate::engine::state(ctx).timezone.borrow_mut() = value.as_deref().map(crate::engine::timezone_named);
    }
    Ok(JsValue::undefined())
}

// ---------------------------------------------------------------------------
// process.stdin (Node mode): one blocking read of fd 0 per request, on a reader thread
// ---------------------------------------------------------------------------

static STDIN_READER: std::sync::Mutex<Option<std::sync::mpsc::Sender<crate::engine::Poster>>> = std::sync::Mutex::new(None);

/// `ops.stdinRead()`: read one chunk of fd 0; the result arrives as `internal.stdinEvent(kind,
/// bytes)` with kind "data", "end" or "error".
fn op_stdin_read(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let st = state(ctx);
    if !st.shared.options.stdin {
        return Ok(JsValue::from(false));
    }
    let poster = st.poster();
    let mut guard = STDIN_READER.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        let (tx, rx) = std::sync::mpsc::channel::<crate::engine::Poster>();
        let spawned = std::thread::Builder::new().name("pi-jsrt-stdin".into()).spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(poster) = rx.recv() {
                // SAFETY: reading fd 0 into our buffer.
                let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
                let (kind, data, errno): (&'static str, Vec<u8>, i32) = if n > 0 {
                    ("data", buf[..n as usize].to_vec(), 0)
                } else if n == 0 {
                    ("end", Vec::new(), 0)
                } else {
                    let e = std::io::Error::last_os_error();
                    if e.kind() == std::io::ErrorKind::Interrupted || e.kind() == std::io::ErrorKind::WouldBlock {
                        // Retry on the next request.
                        ("retry", Vec::new(), 0)
                    } else {
                        ("error", Vec::new(), e.raw_os_error().unwrap_or(libc::EIO))
                    }
                };
                poster.post(move |ctx| {
                    let Some(internal) = crate::engine::try_internal(ctx) else { return };
                    let Ok(f) = internal.get(js_string!("stdinEvent"), ctx) else { return };
                    let Some(f) = f.as_function() else { return };
                    let payload = if kind == "error" {
                        let e = std::io::Error::from_raw_os_error(errno);
                        crate::error::to_js(&super::io_error(&e, "read", None), ctx).into_opaque(ctx).unwrap_or_default()
                    } else {
                        super::bytes_to_js(&data, ctx).unwrap_or_default()
                    };
                    if let Err(e) = f.call(&JsValue::undefined(), &[js_str(kind), payload], ctx) {
                        crate::engine::report_uncaught(ctx, e);
                    }
                });
            }
        });
        if spawned.is_err() {
            return Ok(JsValue::from(false));
        }
        *guard = Some(tx);
    }
    let ok = guard.as_ref().is_some_and(|tx| tx.send(poster).is_ok());
    Ok(JsValue::from(ok))
}

/// The terminal settings before the first `setRawMode(true)`, restored at exit.
static ORIGINAL_TERMIOS: std::sync::Mutex<Option<libc::termios>> = std::sync::Mutex::new(None);

/// `ops.setRawMode(fd, raw) -> errno`: libuv's UV_TTY_MODE_RAW / UV_TTY_MODE_NORMAL.
fn op_set_raw_mode(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let fd = arg_f64(args, 0, ctx)? as i32;
    let raw = args.get_or_undefined(1).to_boolean();
    if !state(ctx).shared.options.stdin {
        return Ok(JsValue::from(0));
    }
    let mut orig = ORIGINAL_TERMIOS.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: termios is plain data filled by tcgetattr.
    unsafe {
        let mut current: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut current) != 0 {
            return Ok(JsValue::from(-std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)));
        }
        if orig.is_none() {
            *orig = Some(current);
        }
        let mut next = orig.unwrap_or(current);
        if raw {
            next.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
            next.c_oflag |= libc::ONLCR;
            next.c_cflag |= libc::CS8;
            next.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
            next.c_cc[libc::VMIN] = 1;
            next.c_cc[libc::VTIME] = 0;
        }
        if libc::tcsetattr(fd, libc::TCSADRAIN, &next) != 0 {
            return Ok(JsValue::from(-std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)));
        }
    }
    Ok(JsValue::from(0))
}

/// Restore the terminal if `setRawMode` changed it (Node's uv_tty_reset_mode at exit).
pub(crate) fn reset_tty_mode() {
    if let Some(orig) = ORIGINAL_TERMIOS.lock().unwrap_or_else(|e| e.into_inner()).take() {
        // SAFETY: restoring settings read from the same descriptor.
        unsafe { libc::tcsetattr(0, libc::TCSADRAIN, &orig) };
    }
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("stdinRead", 0, op_stdin_read),
    ("setRawMode", 2, op_set_raw_mode),
    ("exit", 1, op_exit),
    ("kill", 2, op_kill),
    ("isatty", 1, op_isatty),
    ("termSize", 1, op_term_size),
    ("memoryUsage", 0, op_memory),
    ("osInfo", 0, op_os_info),
    ("cpuUsage", 0, op_cpu_usage),
    ("setEnv", 2, op_set_env),
];
