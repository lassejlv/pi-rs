//! Child process ops backing `node:child_process`.
//!
//! Async children run on tokio; stdout/stderr chunks and lifecycle events are posted
//! back to the JS thread and dispatched through `internal.childEvent(id, kind, ...)`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::js::JsArray;
use crate::js::{Context, JsArgs, JsObject, JsResult, JsValue, js_string};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::{bytes_arg, bytes_to_js, io_code, object};
use crate::engine::{self, OpFn, Poster, arg_f64, arg_string, js_str, state};

enum StdinMsg {
    Data(Vec<u8>),
    Close,
}

struct ChildCtl {
    pid: i32,
    stdin: Option<mpsc::UnboundedSender<StdinMsg>>,
    detached: bool,
}

thread_local! {
    static CHILDREN: RefCell<HashMap<u64, ChildCtl>> = RefCell::new(HashMap::new());
}

#[derive(Clone, Copy, PartialEq)]
enum Io {
    Pipe,
    Inherit,
    Ignore,
}

fn io_mode(v: &JsValue, ctx: &mut Context) -> JsResult<Io> {
    Ok(match v.to_string(ctx)?.to_std_string_lossy().as_str() {
        "inherit" => Io::Inherit,
        "ignore" => Io::Ignore,
        _ => Io::Pipe,
    })
}

fn stdio(m: Io) -> Stdio {
    match m {
        Io::Pipe => Stdio::piped(),
        Io::Inherit => Stdio::inherit(),
        Io::Ignore => Stdio::null(),
    }
}

struct SpawnOpts {
    file: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Option<Vec<(String, String)>>,
    stdio: [Io; 3],
    detached: bool,
    argv0: Option<String>,
    /// stdio entries past 2 that are socket pairs (`'ipc'` channels and `'pipe'`s): child fd numbers.
    extra: Vec<i32>,
    /// `uid`/`gid` options.
    uid: Option<u32>,
    gid: Option<u32>,
}

fn string_list(v: &JsValue, ctx: &mut Context) -> JsResult<Vec<String>> {
    let Some(obj) = v.as_object() else { return Ok(Vec::new()) };
    let arr = JsArray::from_object(obj)?;
    let len = arr.length(ctx)?;
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        out.push(arr.get(i, ctx)?.to_string(ctx)?.to_std_string_lossy());
    }
    Ok(out)
}

fn read_opts(args: &[JsValue], ctx: &mut Context) -> JsResult<SpawnOpts> {
    let file = arg_string(args, 0, ctx)?;
    let argv = string_list(args.get_or_undefined(1), ctx)?;
    let o = args.get_or_undefined(2).as_object().unwrap_or_else(JsObject::with_null_proto);
    let cwd = o.get(js_string!("cwd"), ctx)?;
    let cwd = if cwd.is_null_or_undefined() { None } else { Some(cwd.to_string(ctx)?.to_std_string_lossy()) };
    let env_v = o.get(js_string!("env"), ctx)?;
    let env = if env_v.is_null_or_undefined() {
        None
    } else {
        let flat = string_list(&env_v, ctx)?;
        Some(flat.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect())
    };
    let stdio_v = o.get(js_string!("stdio"), ctx)?;
    let mut modes = [Io::Pipe; 3];
    if let Some(s) = stdio_v.as_object()
        && let Ok(arr) = JsArray::from_object(s)
    {
        for (i, m) in modes.iter_mut().enumerate() {
            let v = arr.get(i as u64, ctx)?;
            if !v.is_null_or_undefined() {
                *m = io_mode(&v, ctx)?;
            }
        }
    }
    let detached = o.get(js_string!("detached"), ctx)?.to_boolean();
    let argv0 = o.get(js_string!("argv0"), ctx)?;
    let argv0 = if argv0.is_null_or_undefined() { None } else { Some(argv0.to_string(ctx)?.to_std_string_lossy()) };
    let extra_v = o.get(js_string!("extra"), ctx)?;
    let mut extra = Vec::new();
    if let Some(e) = extra_v.as_object()
        && let Ok(arr) = JsArray::from_object(e)
    {
        for i in 0..arr.length(ctx)? {
            extra.push(arr.get(i, ctx)?.to_number(ctx)? as i32);
        }
    }
    let num = |v: JsValue, ctx: &mut Context| -> JsResult<Option<u32>> {
        if v.is_null_or_undefined() { Ok(None) } else { Ok(Some(v.to_number(ctx)? as u32)) }
    };
    let uid = num(o.get(js_string!("uid"), ctx)?, ctx)?;
    let gid = num(o.get(js_string!("gid"), ctx)?, ctx)?;
    Ok(SpawnOpts {
        file,
        args: argv,
        cwd,
        env,
        stdio: modes,
        detached,
        argv0,
        extra,
        uid,
        gid,
    })
}

fn apply_common(cmd: &mut std::process::Command, o: &SpawnOpts) {
    use std::os::unix::process::CommandExt;
    cmd.args(&o.args);
    if let Some(cwd) = &o.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(env) = &o.env {
        cmd.env_clear();
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
    }
    if o.detached {
        cmd.process_group(0);
    }
    if let Some(a0) = &o.argv0 {
        cmd.arg0(a0);
    }
    if let Some(uid) = o.uid {
        cmd.uid(uid);
    }
    if let Some(gid) = o.gid {
        cmd.gid(gid);
    }
}

/// Create the socket pairs for extra stdio entries; the child ends are installed at their fd
/// numbers right before exec. Returns the parent ends and the child ends (to close after the
/// spawn).
type ExtraStdio = (Vec<(i32, std::os::unix::net::UnixStream)>, Vec<std::os::unix::net::UnixStream>);

fn extra_stdio(cmd: &mut std::process::Command, o: &SpawnOpts) -> std::io::Result<ExtraStdio> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    if o.extra.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut parents = Vec::new();
    let mut children = Vec::new();
    for &fd in &o.extra {
        let (parent, child) = std::os::unix::net::UnixStream::pair()?;
        parents.push((fd, parent));
        children.push((fd, child));
    }
    let map: Vec<(i32, i32)> = children.iter().map(|(target, s)| (s.as_raw_fd(), *target)).collect();
    // SAFETY: the closure only calls async-signal-safe functions (fcntl, dup2, close). Sources
    // are first moved above every target so installing one cannot clobber another.
    unsafe {
        cmd.pre_exec(move || {
            let mut moved = Vec::with_capacity(map.len());
            for (src, target) in &map {
                let high = libc::fcntl(*src, libc::F_DUPFD_CLOEXEC, 256);
                if high < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                moved.push((high, *target));
            }
            for (src, target) in &moved {
                if libc::dup2(*src, *target) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(*src);
            }
            Ok(())
        });
    }
    Ok((parents, children.into_iter().map(|(_, s)| s).collect()))
}

fn dispatch(ctx: &mut Context, id: u64, args: Vec<JsValue>) {
    let Some(internal) = engine::try_internal(ctx) else { return };
    let Ok(f) = internal.get(js_string!("childEvent"), ctx) else { return };
    let Some(f) = f.as_function() else { return };
    let mut all = vec![JsValue::from(id as f64)];
    all.extend(args);
    if let Err(e) = f.call(&JsValue::undefined(), &all, ctx) {
        engine::report_uncaught(ctx, e);
    }
}

fn spawn_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    tokio: &tokio::runtime::Handle,
    mut r: R,
    poster: Poster,
    id: u64,
    kind: &'static str,
) -> tokio::task::JoinHandle<()> {
    tokio.spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    poster.post(move |ctx| {
                        if let Ok(b) = bytes_to_js(&chunk, ctx) {
                            dispatch(ctx, id, vec![js_str(kind), b]);
                        }
                    });
                }
            }
        }
    })
}

fn signal_name(sig: i32) -> &'static str {
    match sig {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGKILL => "SIGKILL",
        libc::SIGTERM => "SIGTERM",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGABRT => "SIGABRT",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGALRM => "SIGALRM",
        _ => "SIGTERM",
    }
}

fn status_parts(status: &std::process::ExitStatus) -> (JsValue, JsValue) {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(c), _) => (JsValue::from(c), JsValue::null()),
        (None, Some(s)) => (JsValue::null(), js_str(signal_name(s))),
        _ => (JsValue::null(), JsValue::null()),
    }
}

/// `ops.spawn(file, args, opts) -> { id, pid } | { error, code }`
fn op_spawn(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let o = read_opts(args, ctx)?;
    let st = state(ctx);
    let mut std_cmd = std::process::Command::new(&o.file);
    apply_common(&mut std_cmd, &o);
    std_cmd.stdin(stdio(o.stdio[0])).stdout(stdio(o.stdio[1])).stderr(stdio(o.stdio[2]));
    let (extra_parents, extra_children) = match extra_stdio(&mut std_cmd, &o) {
        Ok(v) => v,
        Err(e) => return Err(super::throw_io(ctx, &e, "socketpair", None)),
    };
    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.kill_on_drop(false);
    let spawned = {
        let _guard = st.tokio.enter();
        cmd.spawn()
    };
    drop(cmd);
    drop(extra_children);
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let code = io_code(&e);
            return object(
                ctx,
                vec![
                    ("error", js_str(&format!("spawn {} {code}", o.file))),
                    ("code", js_str(code)),
                    ("errno", JsValue::from(-e.raw_os_error().unwrap_or(0))),
                ],
            );
        }
    };
    let id = st.next_id();
    let pid = child.id().map(|p| p as i32).unwrap_or(0);
    let poster = st.poster();
    let stdin_tx = child.stdin.take().map(|mut stdin| {
        let (tx, mut rx) = mpsc::unbounded_channel::<StdinMsg>();
        st.tokio.spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    StdinMsg::Data(d) => {
                        if stdin.write_all(&d).await.is_err() {
                            break;
                        }
                        let _ = stdin.flush().await;
                    }
                    StdinMsg::Close => break,
                }
            }
            let _ = stdin.shutdown().await;
        });
        tx
    });
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(spawn_reader(&st.tokio, out, poster.clone(), id, "stdout"));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(spawn_reader(&st.tokio, err, poster.clone(), id, "stderr"));
    }
    CHILDREN.with(|c| {
        c.borrow_mut().insert(
            id,
            ChildCtl {
                pid,
                stdin: stdin_tx,
                detached: o.detached,
            },
        )
    });
    st.tokio.spawn(async move {
        let status = child.wait().await;
        let exit_status = status.as_ref().ok().copied();
        poster.post(move |ctx| {
            let (code, sig) = exit_status.as_ref().map(status_parts).unwrap_or((JsValue::null(), JsValue::null()));
            dispatch(ctx, id, vec![js_str("exit"), code, sig]);
        });
        for r in readers {
            let _ = r.await;
        }
        poster.post(move |ctx| {
            CHILDREN.with(|c| c.borrow_mut().remove(&id));
            let (code, sig) = exit_status.as_ref().map(status_parts).unwrap_or((JsValue::null(), JsValue::null()));
            dispatch(ctx, id, vec![js_str("close"), code, sig]);
        });
    });
    let mut extra = Vec::new();
    for (fd, parent) in extra_parents {
        let channel = super::ipc::start(ctx, parent).map_err(|e| super::throw_io(ctx, &e, "socketpair", None))?;
        extra.push(JsValue::from(fd));
        extra.push(JsValue::from(channel as f64));
    }
    let extra = super::array(ctx, extra);
    object(ctx, vec![("id", JsValue::from(id as f64)), ("pid", JsValue::from(pid)), ("extra", extra)])
}

fn op_child_write(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let data = bytes_arg(args.get_or_undefined(1), ctx)?;
    let ok = CHILDREN.with(|c| {
        c.borrow()
            .get(&id)
            .and_then(|ch| ch.stdin.as_ref())
            .is_some_and(|tx| tx.send(StdinMsg::Data(data)).is_ok())
    });
    Ok(JsValue::from(ok))
}

fn op_child_close_stdin(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    CHILDREN.with(|c| {
        if let Some(ch) = c.borrow_mut().get_mut(&id)
            && let Some(tx) = ch.stdin.take()
        {
            let _ = tx.send(StdinMsg::Close);
        }
    });
    Ok(JsValue::undefined())
}

fn op_child_kill(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let sig = args.get_or_undefined(1).to_number(ctx).unwrap_or(15.0) as i32;
    let pid = CHILDREN.with(|c| c.borrow().get(&id).map(|ch| ch.pid));
    let Some(pid) = pid else { return Ok(JsValue::from(false)) };
    // SAFETY: plain syscall.
    let r = unsafe { libc::kill(pid, sig) };
    Ok(JsValue::from(r == 0))
}

/// `ops.spawnSync(file, args, opts, input, timeoutMs, maxBuffer)`
fn op_spawn_sync(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let o = read_opts(args, ctx)?;
    let input = bytes_arg(args.get_or_undefined(3), ctx)?;
    let timeout = args.get_or_undefined(4).to_number(ctx).unwrap_or(0.0);
    let kill_signal = args.get_or_undefined(5).to_number(ctx).unwrap_or(15.0) as i32;
    let mut cmd = std::process::Command::new(&o.file);
    apply_common(&mut cmd, &o);
    let stdin_mode = if o.stdio[0] == Io::Pipe { Io::Pipe } else { o.stdio[0] };
    cmd.stdin(stdio(stdin_mode)).stdout(stdio(o.stdio[1])).stderr(stdio(o.stdio[2]));
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let code = io_code(&e);
            return object(
                ctx,
                vec![
                    ("error", js_str(&format!("spawnSync {} {code}", o.file))),
                    ("code", js_str(code)),
                    ("errno", JsValue::from(-e.raw_os_error().unwrap_or(0))),
                ],
            );
        }
    };
    let pid = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(&input);
        });
    }
    let read_all = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out_t = read_all(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let err_t = read_all(child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(_) => break None,
        }
        if timeout > 0.0 && start.elapsed() > Duration::from_millis(timeout as u64) && !timed_out {
            timed_out = true;
            // SAFETY: plain syscall.
            unsafe { libc::kill(pid as i32, kill_signal) };
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    let (code, sig) = status.as_ref().map(status_parts).unwrap_or((JsValue::null(), JsValue::null()));
    let stdout = bytes_to_js(&stdout, ctx)?;
    let stderr = bytes_to_js(&stderr, ctx)?;
    let mut fields = vec![
        ("pid", JsValue::from(pid)),
        ("status", code),
        ("signal", sig),
        ("stdout", stdout),
        ("stderr", stderr),
    ];
    if timed_out {
        fields.push(("error", js_str(&format!("spawnSync {} ETIMEDOUT", o.file))));
        fields.push(("code", js_str("ETIMEDOUT")));
    }
    object(ctx, fields)
}

pub(crate) fn teardown(_ctx: &mut Context) {
    let children = CHILDREN.with(|c| std::mem::take(&mut *c.borrow_mut()));
    for (_, ch) in children {
        if !ch.detached && ch.pid > 0 {
            // SAFETY: plain syscall.
            unsafe { libc::kill(ch.pid, libc::SIGTERM) };
        }
    }
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("spawn", 3, op_spawn),
    ("childWrite", 2, op_child_write),
    ("childCloseStdin", 1, op_child_close_stdin),
    ("childKill", 2, op_child_kill),
    ("spawnSync", 6, op_spawn_sync),
];
