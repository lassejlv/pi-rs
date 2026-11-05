//! Node-like process mode: run a script the way `node script.js args...` does.
//!
//! There is no `node` binary at runtime, so `child_process.fork()`, `cluster.fork()` and
//! worker-like helpers start another runtime by re-executing the current program with
//! [`NODE_MARKER`] as its first argument (see [`crate::RuntimeOptions::node_exec`]). An
//! embedding binary calls [`main_if_requested`] first thing in `main`; the `pi` binary does.
//!
//! The process behaves like Node's: the main module (ES module or CommonJS, `.ts` included)
//! runs, then the event loop runs until nothing keeps it alive (referenced timers, sockets,
//! servers, child processes, the IPC channel with listeners...), `beforeExit` and `exit` are
//! emitted and the process exits with `process.exitCode`. An uncaught exception or unhandled
//! rejection without listeners prints the error like Node and exits with code 1. When started
//! with an IPC channel (`NODE_CHANNEL_FD`), `process.send()`, `process.connected`,
//! `process.disconnect()` and the `message`/`disconnect` events work as in a forked Node child.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::runtime::{JsRuntime, RuntimeOptions};

/// First argument that makes [`main_if_requested`] run the process in Node mode.
pub const NODE_MARKER: &str = "__pi_jsrt_node__";

/// Environment variables Node consumes at startup (and removes from `process.env`).
const STARTUP_ENV: &[&str] = &["NODE_CHANNEL_FD", "NODE_CHANNEL_SERIALIZATION_MODE", "NODE_UNIQUE_ID"];

/// If the process was started as `<exe> __pi_jsrt_node__ [node options] <script> [args]`, run it
/// in Node mode and return the exit code (the caller exits with it). Otherwise `None`.
pub fn main_if_requested() -> Option<i32> {
    let mut args = std::env::args_os();
    args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new(NODE_MARKER)) {
        return None;
    }
    Some(run(args.map(|a| a.to_string_lossy().into_owned()).collect()))
}

struct Parsed {
    exec_argv: Vec<String>,
    eval: Option<String>,
    print: bool,
    script: Option<String>,
    args: Vec<String>,
}

fn parse(args: Vec<String>) -> Parsed {
    let mut p = Parsed { exec_argv: Vec::new(), eval: None, print: false, script: None, args: Vec::new() };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-e" | "--eval" | "-p" | "--print" => {
                p.print |= a == "-p" || a == "--print";
                p.exec_argv.push(a.to_string());
                if let Some(code) = args.get(i + 1) {
                    p.exec_argv.push(code.clone());
                    p.eval = Some(code.clone());
                }
                i += 2;
            }
            "--" => {
                i += 1;
                break;
            }
            "-r" | "--require" | "--import" | "--loader" | "--experimental-loader" | "--conditions" | "-C" => {
                p.exec_argv.push(a.to_string());
                if let Some(v) = args.get(i + 1) {
                    p.exec_argv.push(v.clone());
                }
                i += 2;
            }
            _ if a.starts_with('-') && a.len() > 1 => {
                p.exec_argv.push(a.to_string());
                i += 1;
            }
            _ => break,
        }
    }
    let mut rest = args.into_iter().skip(i);
    if p.eval.is_none() {
        p.script = rest.next();
    }
    p.args = rest.collect();
    p
}

/// Run in Node mode with `args` (everything after the program name and the marker) and return
/// the exit code. Normally the process exits from inside (`process.exit`, uncaught errors).
pub fn run(args: Vec<String>) -> i32 {
    run_with(args, |_| {}, |_| async { Ok(()) })
}

/// [`run`] for an embedder that adds to the runtime: `configure` edits the options (aliases,
/// virtual modules...) and `setup` runs on the new runtime before the main module.
pub fn run_with<F>(
    args: Vec<String>,
    configure: impl FnOnce(&mut RuntimeOptions),
    setup: impl FnOnce(JsRuntime) -> F,
) -> i32
where
    F: std::future::Future<Output = std::result::Result<(), String>>,
{
    reset_signals();
    let parsed = parse(args);
    let tokio = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("pi-jsrt-io").enable_all().build() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("failed to start tokio runtime: {e}");
            return 1;
        }
    };
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "node".into());
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
    let mut env: HashMap<String, String> = std::env::vars().collect();
    let startup: HashMap<&str, String> = STARTUP_ENV.iter().filter_map(|k| env.remove(*k).map(|v| (*k, v))).collect();
    let script = parsed.script.as_ref().map(|s| {
        let p = std::path::Path::new(s);
        if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
    });
    // `--import`/`--require` preloads run before the main module; `--input-type=module` evaluates
    // `-e` code as an ES module (top-level await, static imports).
    let mut preloads: Vec<(bool, String)> = Vec::new();
    let mut flags = parsed.exec_argv.iter();
    while let Some(flag) = flags.next() {
        match flag.as_str() {
            "-r" | "--require" | "--import" => {
                if let Some(spec) = flags.next() {
                    preloads.push((flag != "--import", spec.clone()));
                }
            }
            f if f.starts_with("--import=") => preloads.push((false, f["--import=".len()..].to_string())),
            f if f.starts_with("--require=") => preloads.push((true, f["--require=".len()..].to_string())),
            _ => {}
        }
    }
    let module_eval = parsed.exec_argv.iter().any(|a| a == "--input-type=module");
    let (eval, module_source) = if module_eval { (None, parsed.eval.clone()) } else { (parsed.eval.clone(), None) };
    let eval_name = cwd.join("[eval1]").display().to_string();
    let mut argv = vec![exe];
    if let Some(s) = &script {
        argv.push(s.display().to_string());
    }
    argv.extend(parsed.args.iter().cloned());
    let mut options = RuntimeOptions {
        cwd: Some(cwd),
        env: Some(env),
        argv,
        exec_argv: parsed.exec_argv.clone(),
        esm_cjs_globals: false,
        stdin: true,
        expose_gc: parsed.exec_argv.iter().any(|a| a == "--expose-gc" || a == "--expose_gc"),
        tokio: Some(tokio.handle().clone()),
        on_exit: Some(Arc::new(|code| {
            use std::io::Write;
            crate::ops::process::reset_tty_mode();
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            std::process::exit(code);
        })),
        ..Default::default()
    };
    configure(&mut options);
    tokio.block_on(async move {
        let rt = match JsRuntime::new(options) {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        };
        if let Err(e) = setup(rt.clone()).await {
            eprintln!("{e}");
            return 1;
        }
        for (require, spec) in &preloads {
            let loaded = if *require { rt.require(spec.as_str()).await.map(drop) } else { rt.import(spec.as_str()).await.map(drop) };
            if let Err(e) = loaded {
                eprintln!("{e}");
                return 1;
            }
        }
        let main = json!({
            "file": script.map(|s| s.display().to_string()),
            "eval": eval,
            "print": parsed.print,
            "ipcFd": startup.get("NODE_CHANNEL_FD").and_then(|v| v.parse::<i64>().ok()),
            "serialization": startup.get("NODE_CHANNEL_SERIALIZATION_MODE").cloned().unwrap_or_else(|| "json".into()),
            "uniqueId": startup.get("NODE_UNIQUE_ID"),
        });
        if call_internal(&rt, "nodeMain", main).await.is_err() {
            return 1;
        }
        if let Some(source) = module_source
            && let Err(e) = rt.eval_module(source, eval_name).await
        {
            eprintln!("{e}");
            return 1;
        }
        loop {
            if rt.wait_idle().await.is_err() {
                return 1;
            }
            match call_internal(&rt, "nodeBeforeExit", Value::Null).await {
                Ok(Value::Bool(true)) => continue,
                Ok(_) => break,
                Err(_) => return 1,
            }
        }
        let code = match call_internal(&rt, "nodeExit", Value::Null).await {
            Ok(v) => v.as_i64().unwrap_or(0) as i32,
            Err(_) => 1,
        };
        crate::ops::process::reset_tty_mode();
        code
    })
}

/// Like Node's `PlatformInit`: start from default signal dispositions (SIGPIPE and SIGXFSZ
/// ignored) and an empty signal mask, whatever the parent left behind.
fn reset_signals() {
    // SAFETY: plain sigaction/sigprocmask calls at startup, before other threads exist.
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        libc::sigemptyset(&mut act.sa_mask);
        for nr in 1..32 {
            if nr == libc::SIGKILL || nr == libc::SIGSTOP {
                continue;
            }
            act.sa_sigaction = if nr == libc::SIGPIPE || nr == libc::SIGXFSZ { libc::SIG_IGN } else { libc::SIG_DFL };
            libc::sigaction(nr, &act, std::ptr::null_mut());
        }
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
    }
}

/// Call `internal[name](arg)` (awaiting a returned promise).
async fn call_internal(rt: &JsRuntime, name: &'static str, arg: Value) -> crate::Result<Value> {
    rt.call_internal(name, arg).await
}
