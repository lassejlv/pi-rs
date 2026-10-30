//! Message ports that can cross runtimes, and `worker_threads` workers.
//!
//! Every `MessagePort` (from `MessageChannel`, a worker's `parentPort`, ...) is an entry in a
//! process-wide registry. A message is the V8 serialization of the value (`js/node/v8.js`) plus
//! the ids of transferred ports and the bytes of transferred `ArrayBuffer`s; it is queued on the
//! receiving port and the runtime that owns that port is woken (`internal.portEvent(id,
//! "wake")`) to drain the queue (`ops.portRecv`). A transferred port has no owner until the
//! receiving runtime deserializes the message and attaches it; messages sent meanwhile wait in
//! its queue.
//!
//! A `Worker` is another [`JsRuntime`] on its own thread that shares the parent's tokio
//! runtime. Its stdout/stderr, uncaught errors and exit code are reported to the parent with
//! `internal.workerEvent(id, kind, ...)`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use crate::js::JsArray;
use crate::js::{Context, JsArgs, JsResult, JsValue, js_string};
use parking_lot::Mutex;

use super::{bytes_arg, bytes_to_js, object};
use crate::engine::{self, OpFn, Poster, arg_f64, js_str, state};
use crate::runtime::{JsRuntime, RuntimeOptions, VirtualModule};

pub(crate) struct PortMsg {
    bytes: Vec<u8>,
    ports: Vec<u64>,
    buffers: Vec<Vec<u8>>,
}

#[derive(Clone)]
struct Owner {
    poster: Poster,
    runtime: usize,
}

struct Port {
    peer: u64,
    owner: Option<Owner>,
    queue: VecDeque<PortMsg>,
    wake_pending: bool,
    closed: bool,
}

static PORTS: LazyLock<Mutex<HashMap<u64, Port>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_PORT: AtomicU64 = AtomicU64::new(1);
static NEXT_THREAD_ID: AtomicU64 = AtomicU64::new(1);

fn runtime_key(ctx: &Context) -> usize {
    Arc::as_ptr(&state(ctx).shared) as usize
}

fn owner(ctx: &Context) -> Owner {
    Owner { poster: state(ctx).poster(), runtime: runtime_key(ctx) }
}

fn dispatch(ctx: &mut Context, name: &'static str, args: Vec<JsValue>) {
    let Some(internal) = engine::try_internal(ctx) else { return };
    let Ok(f) = internal.get(js_string!(name), ctx) else { return };
    let Some(f) = f.as_function() else { return };
    if let Err(e) = f.call(&JsValue::undefined(), &args, ctx) {
        engine::report_uncaught(ctx, e);
    }
}

/// Wake the owner of `id` (if any) to drain its queue; at most one wake is in flight.
fn wake(ports: &mut HashMap<u64, Port>, id: u64) {
    let Some(port) = ports.get_mut(&id) else { return };
    if port.wake_pending || port.queue.is_empty() {
        return;
    }
    if let Some(owner) = &port.owner {
        port.wake_pending = true;
        owner.poster.post(move |ctx| {
            if let Some(p) = PORTS.lock().get_mut(&id) {
                p.wake_pending = false;
            }
            dispatch(ctx, "portEvent", vec![JsValue::from(id as f64), js_str("wake")]);
        });
    }
}

fn notify_close(port: &Port, id: u64) {
    if let Some(owner) = &port.owner {
        owner.poster.post(move |ctx| dispatch(ctx, "portEvent", vec![JsValue::from(id as f64), js_str("close")]));
    }
}

/// Create an entangled pair owned by the current runtime.
pub(crate) fn create_pair(ctx: &Context) -> (u64, u64) {
    let a = NEXT_PORT.fetch_add(2, Ordering::Relaxed);
    let b = a + 1;
    let o = owner(ctx);
    let mut ports = PORTS.lock();
    for (id, peer) in [(a, b), (b, a)] {
        ports.insert(id, Port { peer, owner: Some(o.clone()), queue: VecDeque::new(), wake_pending: false, closed: false });
    }
    (a, b)
}

fn close_port(id: u64) {
    let mut ports = PORTS.lock();
    let Some(port) = ports.remove(&id) else { return };
    notify_close(&port, id);
    if let Some(peer) = ports.get_mut(&port.peer) {
        peer.closed = true;
        let peer_id = port.peer;
        // The peer is closed too once its queued messages are delivered (Node closes both ends).
        if let Some(owner) = &peer.owner {
            owner.poster.post(move |ctx| dispatch(ctx, "portEvent", vec![JsValue::from(peer_id as f64), js_str("peerClosed")]));
        }
    }
}

fn op_port_create_pair(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (a, b) = create_pair(ctx);
    Ok(super::array(ctx, vec![JsValue::from(a as f64), JsValue::from(b as f64)]))
}

fn id_list(v: &JsValue, ctx: &mut Context) -> JsResult<Vec<u64>> {
    let Some(obj) = v.as_object() else { return Ok(Vec::new()) };
    let arr = JsArray::from_object(obj)?;
    let mut out = Vec::new();
    for i in 0..arr.length(ctx)? {
        out.push(arr.get(i, ctx)?.to_number(ctx)? as u64);
    }
    Ok(out)
}

fn buffer_list(v: &JsValue, ctx: &mut Context) -> JsResult<Vec<Vec<u8>>> {
    let Some(obj) = v.as_object() else { return Ok(Vec::new()) };
    let arr = JsArray::from_object(obj)?;
    let mut out = Vec::new();
    for i in 0..arr.length(ctx)? {
        let b = arr.get(i, ctx)?;
        out.push(bytes_arg(&b, ctx)?);
    }
    Ok(out)
}

/// `ops.portSend(id, bytes, portIds, buffers) -> bool` (false when the peer is gone).
fn op_port_send(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let bytes = bytes_arg(args.get_or_undefined(1), ctx)?;
    let ports_v = id_list(args.get_or_undefined(2), ctx)?;
    let buffers = buffer_list(args.get_or_undefined(3), ctx)?;
    let mut ports = PORTS.lock();
    let Some(peer) = ports.get(&id).map(|p| p.peer) else { return Ok(JsValue::from(false)) };
    let Some(target) = ports.get_mut(&peer) else { return Ok(JsValue::from(false)) };
    target.queue.push_back(PortMsg { bytes, ports: ports_v, buffers });
    wake(&mut ports, peer);
    Ok(JsValue::from(true))
}

/// `ops.portRecv(id) -> [bytes, portIds, buffers] | undefined`
fn op_port_recv(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let msg = PORTS.lock().get_mut(&id).and_then(|p| p.queue.pop_front());
    let Some(msg) = msg else { return Ok(JsValue::undefined()) };
    let bytes = bytes_to_js(&msg.bytes, ctx)?;
    let ports: Vec<JsValue> = msg.ports.iter().map(|p| JsValue::from(*p as f64)).collect();
    let ports = super::array(ctx, ports);
    let mut buffers = Vec::new();
    for b in &msg.buffers {
        let ab = crate::js::JsArrayBuffer::new(b.len(), ctx)?;
        if let Some(data) = ab.data_mut() {
            data.copy_from_slice(b);
        }
        buffers.push(JsValue::from(ab));
    }
    let buffers = super::array(ctx, buffers);
    Ok(super::array(ctx, vec![bytes, ports, buffers]))
}

/// `ops.portDetach(id)`: the port is being transferred away; queue its messages.
fn op_port_detach(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    if let Some(p) = PORTS.lock().get_mut(&id) {
        p.owner = None;
        p.wake_pending = false;
    }
    Ok(JsValue::undefined())
}

/// `ops.portAttach(id) -> bool`: the current runtime owns the port from now on.
fn op_port_attach(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let o = owner(ctx);
    let mut ports = PORTS.lock();
    let Some(p) = ports.get_mut(&id) else { return Ok(JsValue::from(false)) };
    p.owner = Some(o);
    let closed = p.closed;
    wake(&mut ports, id);
    Ok(JsValue::from(!closed || ports.get(&id).is_some_and(|p| !p.queue.is_empty())))
}

/// `ops.portWake(id)`: re-check the queue (after `start()`).
fn op_port_wake(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    wake(&mut PORTS.lock(), id);
    Ok(JsValue::undefined())
}

/// `ops.portClose(id)`: close the port and its peer.
fn op_port_close(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    close_port(id);
    Ok(JsValue::undefined())
}

// ---------------------------------------------------------------------------
// BroadcastChannel across runtimes
// ---------------------------------------------------------------------------

struct Subscriber {
    id: u64,
    owner: Owner,
}

static BROADCAST: LazyLock<Mutex<HashMap<String, Vec<Subscriber>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `ops.broadcastJoin(name) -> id`
fn op_broadcast_join(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = engine::arg_string(args, 0, ctx)?;
    let id = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
    BROADCAST.lock().entry(name).or_default().push(Subscriber { id, owner: owner(ctx) });
    Ok(JsValue::from(id as f64))
}

/// `ops.broadcastLeave(name, id)`
fn op_broadcast_leave(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = engine::arg_string(args, 0, ctx)?;
    let id = arg_f64(args, 1, ctx)? as u64;
    let mut all = BROADCAST.lock();
    if let Some(list) = all.get_mut(&name) {
        list.retain(|s| s.id != id);
        if list.is_empty() {
            all.remove(&name);
        }
    }
    Ok(JsValue::undefined())
}

/// `ops.broadcastPost(name, fromId, bytes)`: deliver to every other subscriber.
fn op_broadcast_post(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = engine::arg_string(args, 0, ctx)?;
    let from = arg_f64(args, 1, ctx)? as u64;
    let bytes = bytes_arg(args.get_or_undefined(2), ctx)?;
    let all = BROADCAST.lock();
    for s in all.get(&name).map(|l| l.as_slice()).unwrap_or(&[]) {
        if s.id == from {
            continue;
        }
        let (id, bytes) = (s.id, bytes.clone());
        s.owner.poster.post(move |ctx| {
            if let Ok(b) = bytes_to_js(&bytes, ctx) {
                dispatch(ctx, "broadcastEvent", vec![JsValue::from(id as f64), b]);
            }
        });
    }
    Ok(JsValue::undefined())
}

// ---------------------------------------------------------------------------
// Workers
// ---------------------------------------------------------------------------

struct WorkerCtl {
    parent: usize,
    rt: Option<JsRuntime>,
    terminated: bool,
    exit_code: Option<i32>,
}

/// Worker runtime (by runtime key) -> (parent poster, worker id), for reports from the worker.
static LINKS: LazyLock<Mutex<HashMap<usize, (Poster, u64)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

static WORKERS: LazyLock<Mutex<HashMap<u64, Arc<Mutex<WorkerCtl>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn get_str(ctx: &mut Context, o: &JsValue, key: &str) -> JsResult<Option<String>> {
    let Some(obj) = o.as_object() else { return Ok(None) };
    let v = obj.get(js_str(key).as_string().unwrap().clone(), ctx)?;
    if v.is_null_or_undefined() { Ok(None) } else { Ok(Some(v.to_string(ctx)?.to_std_string_lossy())) }
}

fn get_list(ctx: &mut Context, o: &JsValue, key: &str) -> JsResult<Vec<String>> {
    let Some(obj) = o.as_object() else { return Ok(Vec::new()) };
    let v = obj.get(js_str(key).as_string().unwrap().clone(), ctx)?;
    let Some(arr) = v.as_object() else { return Ok(Vec::new()) };
    let arr = JsArray::from_object(arr)?;
    let mut out = Vec::new();
    for i in 0..arr.length(ctx)? {
        out.push(arr.get(i, ctx)?.to_string(ctx)?.to_std_string_lossy());
    }
    Ok(out)
}

/// `ops.workerStart({ file, code, argv, execArgv, env: [k, v, ...], name, port, data, dataPorts,
/// dataBuffers, stdout, stderr }) -> { id, threadId }`
fn op_worker_start(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let o = args.get_or_undefined(0).clone();
    let file = get_str(ctx, &o, "file")?;
    let code = get_str(ctx, &o, "code")?;
    let argv = get_list(ctx, &o, "argv")?;
    let exec_argv = get_list(ctx, &o, "execArgv")?;
    let env_flat = get_list(ctx, &o, "env")?;
    let name = get_str(ctx, &o, "name")?.unwrap_or_default();
    let obj = o.as_object().ok_or_else(|| engine::type_error("worker options"))?;
    let port = obj.get(js_string!("port"), ctx)?.to_number(ctx)? as u64;
    let data = bytes_arg(&obj.get(js_string!("data"), ctx)?, ctx)?;
    let data_ports = id_list(&obj.get(js_string!("dataPorts"), ctx)?, ctx)?;
    let data_buffers = buffer_list(&obj.get(js_string!("dataBuffers"), ctx)?, ctx)?;
    let limits = obj.get(js_string!("limits"), ctx)?;
    let limits = crate::convert::to_json(&limits, ctx).unwrap_or(serde_json::Value::Null);
    let env: HashMap<String, String> = env_flat.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect();

    let st = state(ctx);
    let parent = st.shared.clone();
    let id = st.next_id();
    let thread_id = NEXT_THREAD_ID.fetch_add(1, Ordering::Relaxed);
    let poster = st.poster();
    let tokio = st.tokio.clone();
    let cwd = st.cwd.borrow().clone();
    let ctl = Arc::new(Mutex::new(WorkerCtl { parent: runtime_key(ctx), rt: None, terminated: false, exit_code: None }));
    WORKERS.lock().insert(id, ctl.clone());
    st.loop_ref(1);

    let out_poster = poster.clone();
    let err_poster = poster.clone();
    let virtual_modules: HashMap<String, VirtualModule> = parent
        .virtual_modules
        .read()
        .iter()
        .filter(|(_, m)| !matches!(m, VirtualModule::Object(_)))
        .map(|(k, m)| (k.clone(), m.clone()))
        .collect();
    let exit_ctl = ctl.clone();
    let options = RuntimeOptions {
        cwd: Some(cwd),
        env: Some(env),
        argv,
        exec_argv,
        tokio: Some(tokio.clone()),
        stdout: Some(Arc::new(move |b: &[u8]| {
            let b = b.to_vec();
            out_poster.post(move |ctx| {
                if let Ok(v) = bytes_to_js(&b, ctx) {
                    dispatch(ctx, "workerEvent", vec![JsValue::from(id as f64), js_str("stdout"), v]);
                }
            });
        })),
        stderr: Some(Arc::new(move |b: &[u8]| {
            let b = b.to_vec();
            err_poster.post(move |ctx| {
                if let Ok(v) = bytes_to_js(&b, ctx) {
                    dispatch(ctx, "workerEvent", vec![JsValue::from(id as f64), js_str("stderr"), v]);
                }
            });
        })),
        on_exit: Some(Arc::new(move |code| {
            let mut c = exit_ctl.lock();
            if c.exit_code.is_none() {
                c.exit_code = Some(code);
            }
            if let Some(rt) = &c.rt {
                rt.shutdown();
            }
        })),
        virtual_modules,
        aliases: parent.aliases.read().clone(),
        thread_name: format!("pi-jsrt-worker-{thread_id}"),
        thread_stack_size: parent.options.thread_stack_size,
        loop_iteration_limit: parent.options.loop_iteration_limit,
        node_exec: parent.options.node_exec.clone(),
        esm_cjs_globals: parent.options.esm_cjs_globals,
        ..Default::default()
    };
    let main = serde_json::json!({
        "file": file,
        "code": code,
        "name": name,
        "threadId": thread_id,
        "port": port,
        "data": data,
        "dataPorts": data_ports,
        "dataBuffers": data_buffers,
        "limits": limits,
    });
    let driver_poster = poster.clone();
    let spawned = std::thread::Builder::new().name(format!("pi-jsrt-worker-driver-{thread_id}")).spawn(move || {
        let report_exit = |code: i32| {
            driver_poster.post(move |ctx| {
                WORKERS.lock().remove(&id);
                state(ctx).loop_ref(-1);
                dispatch(ctx, "workerEvent", vec![JsValue::from(id as f64), js_str("exit"), JsValue::from(code)]);
            });
        };
        let rt = match JsRuntime::new(options) {
            Ok(rt) => rt,
            Err(e) => {
                let msg = e.to_string();
                driver_poster.post(move |ctx| dispatch(ctx, "workerEvent", vec![JsValue::from(id as f64), js_str("startError"), js_str(&msg)]));
                report_exit(1);
                return;
            }
        };
        {
            let mut c = ctl.lock();
            if c.terminated {
                rt.shutdown();
            }
            c.rt = Some(rt.clone());
        }
        let key = Arc::as_ptr(&rt.inner.shared) as usize;
        LINKS.lock().insert(key, (driver_poster.clone(), id));
        driver_poster.post(move |ctx| dispatch(ctx, "workerEvent", vec![JsValue::from(id as f64), js_str("online")]));
        let code = tokio.block_on(async {
            rt.call_internal("loadGlue", serde_json::json!("node/worker_threads.js")).await.ok()?;
            if let Err(e) = rt.call_internal("workerMain", main).await {
                tracing::debug!(target: "pi_jsrt", "worker main failed: {e}");
                return None;
            }
            rt.wait_idle().await.ok()?;
            rt.call_internal("workerExit", serde_json::Value::Null).await.ok().and_then(|v| v.as_i64()).map(|c| c as i32)
        });
        LINKS.lock().remove(&key);
        let (terminated, explicit) = {
            let mut c = ctl.lock();
            c.rt = None;
            (c.terminated, c.exit_code)
        };
        rt.shutdown();
        drop(rt);
        let code = if terminated { 1 } else { explicit.or(code).unwrap_or(1) };
        report_exit(code);
    });
    if let Err(e) = spawned {
        return Err(super::throw_io(ctx, &e, "uv_thread_create", None));
    }
    object(ctx, vec![("id", JsValue::from(id as f64)), ("threadId", JsValue::from(thread_id as f64))])
}

/// `ops.workerReport(kind, bytes)`: from inside a worker, send an event to the parent's `Worker`.
fn op_worker_report(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let kind = engine::arg_string(args, 0, ctx)?;
    let bytes = bytes_arg(args.get_or_undefined(1), ctx)?;
    let link = LINKS.lock().get(&runtime_key(ctx)).cloned();
    if let Some((poster, id)) = link {
        poster.post(move |ctx| {
            if let Ok(v) = bytes_to_js(&bytes, ctx) {
                dispatch(ctx, "workerEvent", vec![JsValue::from(id as f64), js_str(&kind), v]);
            }
        });
    }
    Ok(JsValue::undefined())
}

/// `ops.workerTerminate(id)`
fn op_worker_terminate(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    if let Some(ctl) = WORKERS.lock().get(&id).cloned() {
        let mut c = ctl.lock();
        c.terminated = true;
        if let Some(rt) = &c.rt {
            rt.shutdown();
        }
    }
    Ok(JsValue::undefined())
}

/// `ops.workerRef(delta)`: a worker's `ref()`/`unref()` adjust the parent's loop.
fn op_worker_ref(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let delta = arg_f64(args, 0, ctx)? as i64;
    state(ctx).loop_ref(delta);
    Ok(JsValue::undefined())
}

pub(crate) fn teardown(ctx: &mut Context) {
    let me = runtime_key(ctx);
    let children: Vec<Arc<Mutex<WorkerCtl>>> = WORKERS.lock().values().filter(|w| w.lock().parent == me).cloned().collect();
    for w in children {
        let mut c = w.lock();
        c.terminated = true;
        if let Some(rt) = &c.rt {
            rt.shutdown();
        }
    }
    let owned: Vec<u64> = PORTS.lock().iter().filter(|(_, p)| p.owner.as_ref().is_some_and(|o| o.runtime == me)).map(|(id, _)| *id).collect();
    for id in owned {
        close_port(id);
    }
    BROADCAST.lock().values_mut().for_each(|l| l.retain(|s| s.owner.runtime != me));
}

pub(crate) const OPS: &[(&str, usize, OpFn)] = &[
    ("portCreatePair", 0, op_port_create_pair),
    ("portSend", 4, op_port_send),
    ("portRecv", 1, op_port_recv),
    ("portDetach", 1, op_port_detach),
    ("portAttach", 1, op_port_attach),
    ("portWake", 1, op_port_wake),
    ("portClose", 1, op_port_close),
    ("broadcastJoin", 1, op_broadcast_join),
    ("broadcastLeave", 2, op_broadcast_leave),
    ("broadcastPost", 3, op_broadcast_post),
    ("workerStart", 1, op_worker_start),
    ("workerTerminate", 1, op_worker_terminate),
    ("workerReport", 2, op_worker_report),
    ("workerRef", 1, op_worker_ref),
];
