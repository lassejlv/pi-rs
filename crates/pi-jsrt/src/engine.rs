//! The JS thread: QuickJS engine, job queue, timers, handle table and event loop.
//!
//! Everything in this module runs on the dedicated runtime thread. The public,
//! thread-safe API lives in [`crate::runtime`] and talks to this thread by
//! posting [`Msg`]s.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use crate::js::{
    Context, Engine, EngineOptions, FunctionObjectBuilder, JsArgs, JsData, JsError, JsFunction, JsNativeError, JsObject, JsPromise, JsResult,
    JsString, JsValue, NativeFunction, PromiseState, ResolvingFunctions, Source, Tracer, js_string,
};

use crate::error::{self, Error};
use crate::js::rquickjs::qjs;
use crate::runtime::{Inner, Shared};
use crate::transpile::SourceMapTable;

/// Work item executed on the JS thread.
pub(crate) type Task = Box<dyn FnOnce(&mut Context) + Send + 'static>;

/// Messages processed by the JS thread's event loop.
pub(crate) enum Msg {
    Task(Task),
    Release(u64),
    Shutdown,
}

/// Value delivered to a Rust caller waiting for a JS result.
pub(crate) enum Settled {
    Json(serde_json::Value),
    Handle(u64),
}

/// Per-thread runtime state, reachable from the context via [`state`].
pub(crate) struct State {
    pub(crate) tx: mpsc::Sender<Msg>,
    pub(crate) rt: Weak<Inner>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) tokio: tokio::runtime::Handle,
    handles: RefCell<HashMap<u64, JsValue>>,
    created: RefCell<Option<Vec<u64>>>,
    pub(crate) host_fns: RefCell<HashMap<u64, JsObject>>,
    timers: RefCell<Timers>,
    pending: RefCell<HashMap<u64, ResolvingFunctions>>,
    pub(crate) source_maps: RefCell<HashMap<String, SourceMapTable>>,
    rejections: RefCell<Vec<JsObject>>,
    pub(crate) internal: RefCell<Option<JsObject>>,
    pub(crate) ops: RefCell<Option<JsObject>>,
    pub(crate) cwd: RefCell<PathBuf>,
    stopping: Cell<bool>,
    /// Handles that keep the event loop alive (sockets, servers, child processes, ports...),
    /// counted by the glue through `ops.loopRef(delta)`, like libuv's active handles.
    loop_refs: Cell<i64>,
    /// The current async context (AsyncLocalStorage frame and async_hooks ids).
    pub(crate) async_ctx: RefCell<AsyncCtx>,
    next_async_id: Cell<f64>,
    /// `internal.asyncHookEmit` while an async hook is enabled.
    pub(crate) async_hook: RefCell<Option<JsObject>>,
    in_async_hook: Cell<bool>,
    /// Event loop timing for `performance.eventLoopUtilization()` / `nodeTiming`: when the loop
    /// started (ms since the runtime started, 0 before) and the time it spent waiting.
    loop_start_ms: Cell<f64>,
    idle_ms: Cell<f64>,
    /// `process.nextTick` callbacks are queued (see [`run_microtasks`]).
    ticks_pending: Cell<bool>,
    /// The runtime's timezone, shared with [`Hooks`]; `ops.setEnv("TZ", …)` changes it.
    pub(crate) timezone: Rc<RefCell<Option<jiff::tz::TimeZone>>>,
    /// Events posted by I/O tasks that have not run yet (see [`Poster`]).
    queued: Arc<std::sync::atomic::AtomicUsize>,
    /// Waiters for [`State::alive`] to become false (see `JsRuntime::wait_idle`).
    idle_waiters: RefCell<Vec<futures::channel::oneshot::Sender<()>>>,
    /// Interrupt-handler calls left before a synchronous run is stopped
    /// (`loop_iteration_limit`), `None` when unlimited.
    budget: Cell<Option<u64>>,
    /// Async contexts saved by promise jobs entering theirs (see [`job_enter`]).
    job_stack: RefCell<Vec<(AsyncCtx, bool)>>,
}

impl State {
    pub(crate) fn next_id(&self) -> u64 {
        self.shared.next_id()
    }

    /// Store a value in the handle table. The entry is "unadopted" until Rust
    /// takes a reference (see [`Shared::adopt`]).
    pub(crate) fn new_handle(&self, value: JsValue) -> u64 {
        let id = self.next_id();
        self.handles.borrow_mut().insert(id, value);
        if let Some(created) = self.created.borrow_mut().as_mut() {
            created.push(id);
        }
        id
    }

    /// Store a value and give Rust one reference to it.
    pub(crate) fn new_owned_handle(&self, value: JsValue) -> u64 {
        let id = self.next_id();
        self.handles.borrow_mut().insert(id, value);
        self.shared.adopt(id);
        id
    }

    pub(crate) fn handle_count(&self) -> usize {
        self.handles.borrow().len()
    }

    pub(crate) fn get_handle(&self, id: u64) -> Option<JsValue> {
        self.handles.borrow().get(&id).cloned()
    }

    fn release(&self, id: u64) {
        if self.shared.release(id) {
            self.handles.borrow_mut().remove(&id);
        }
    }

    /// Drop table entries created during a conversion that Rust did not adopt.
    pub(crate) fn sweep_unadopted(&self, ids: &[u64]) {
        let mut handles = self.handles.borrow_mut();
        for id in ids {
            if !self.shared.is_adopted(*id) {
                handles.remove(id);
            }
        }
    }

    /// Whether anything keeps the event loop alive: referenced timers, pending async ops or
    /// referenced handles (Node's `uv_loop_alive`).
    pub(crate) fn alive(&self) -> bool {
        self.loop_refs.get() > 0
            || !self.pending.borrow().is_empty()
            || self.timers.borrow().refed() > 0
            || self.queued.load(std::sync::atomic::Ordering::SeqCst) > 0
            || crate::ops::signal::in_flight()
    }

    pub(crate) fn loop_ref(&self, delta: i64) {
        self.loop_refs.set(self.loop_refs.get() + delta);
    }

    pub(crate) fn add_idle_waiter(&self, tx: futures::channel::oneshot::Sender<()>) {
        self.idle_waiters.borrow_mut().push(tx);
    }

    fn notify_idle(&self) {
        if self.idle_waiters.borrow().is_empty() || self.alive() {
            return;
        }
        for w in std::mem::take(&mut *self.idle_waiters.borrow_mut()) {
            let _ = w.send(());
        }
    }

    /// Post a task to this thread from anywhere (e.g. a tokio task).
    pub(crate) fn poster(&self) -> Poster {
        Poster { tx: self.tx.clone(), queued: self.queued.clone() }
    }
}

/// Cloneable, `Send` handle for posting tasks to the JS thread.
#[derive(Clone)]
pub(crate) struct Poster {
    tx: mpsc::Sender<Msg>,
    /// Events posted but not yet run: they keep the loop alive (an I/O completion in flight).
    queued: Arc<std::sync::atomic::AtomicUsize>,
}

impl Poster {
    pub(crate) fn post(&self, f: impl FnOnce(&mut Context) + Send + 'static) -> bool {
        self.queued.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let queued = self.queued.clone();
        let ok = self.tx.send(Msg::Task(Box::new(move |ctx| {
            queued.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            f(ctx)
        })));
        if ok.is_err() {
            self.queued.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        ok.is_ok()
    }
}

/// Fetch the thread state from a context.
pub(crate) fn state(ctx: &Context) -> Rc<State> {
    ctx.get_data::<Rc<State>>().expect("pi-jsrt state missing from context").clone()
}

/// The internal JS helper object created by the prelude.
pub(crate) fn internal(ctx: &Context) -> JsObject {
    try_internal(ctx).expect("pi-jsrt prelude not loaded")
}

/// The internal JS helper object, if the prelude has been loaded.
pub(crate) fn try_internal(ctx: &Context) -> Option<JsObject> {
    ctx.get_data::<Rc<State>>()?.internal.borrow().clone()
}

/// Run all pending microtasks, then report unhandled rejections.
pub(crate) fn run_microtasks(ctx: &mut Context) {
    // Node's processTicksAndRejections: the nextTick queue runs before promise jobs, and ticks
    // queued by promise jobs run once the job queue is empty.
    loop {
        run_ticks(ctx);
        while let Some(result) = ctx.run_pending_job() {
            if let Err(err) = result {
                report_uncaught(ctx, err);
            }
        }
        if !state(ctx).ticks_pending.get() {
            break;
        }
    }
    let st = state(ctx);
    let rejected = std::mem::take(&mut *st.rejections.borrow_mut());
    for promise in rejected {
        let Ok(p) = JsPromise::from_object(promise.clone()) else { continue };
        if let PromiseState::Rejected(reason) = p.state() {
            if handled_by_js(ctx, "unhandledRejection", &reason, Some(promise.into())) {
                continue;
            }
            let err = error::from_thrown(&reason, ctx);
            report(ctx, "Unhandled promise rejection", &err);
        }
    }
}

/// Drain `process.nextTick` callbacks (`internal.runTicks`).
fn run_ticks(ctx: &mut Context) {
    let st = state(ctx);
    if !st.ticks_pending.replace(false) {
        return;
    }
    let Some(internal) = try_internal(ctx) else { return };
    let Ok(f) = internal.get(js_string!("runTicks"), ctx) else { return };
    let Some(f) = f.as_function() else { return };
    if let Err(e) = f.call(&JsValue::undefined(), &[], ctx) {
        report_uncaught(ctx, e);
    }
}

/// `ops.loopTiming() -> [loopStartMs, idleMs]`
fn op_loop_timing(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let st = state(ctx);
    let values = [JsValue::from(st.loop_start_ms.get()), JsValue::from(st.idle_ms.get())];
    Ok(crate::js::JsArray::from_iter(values, ctx).into())
}

/// `ops.ticksScheduled()`: the nextTick queue became non-empty.
fn op_ticks_scheduled(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    state(ctx).ticks_pending.set(true);
    Ok(JsValue::undefined())
}

/// Offer an uncaught exception / unhandled rejection to the JS `process` listeners
/// (`internal.onUncaught`, see `js/node/process.js`); true when a listener took it.
fn handled_by_js(ctx: &mut Context, kind: &str, value: &JsValue, promise: Option<JsValue>) -> bool {
    let Some(internal) = try_internal(ctx) else { return false };
    let Ok(f) = internal.get(js_string!("onUncaught"), ctx) else { return false };
    let Some(f) = f.as_function() else { return false };
    let args = [js_str(kind), value.clone(), promise.unwrap_or_default()];
    match f.call(&JsValue::undefined(), &args, ctx) {
        Ok(v) => v.to_boolean(),
        Err(e) => {
            // A throwing listener is itself an uncaught exception (not offered again).
            let err = error::from_js(e, ctx);
            report(ctx, "Uncaught exception", &err);
            true
        }
    }
}

pub(crate) fn report_uncaught(ctx: &mut Context, err: JsError) {
    if let Ok(value) = err.clone().into_opaque(ctx)
        && handled_by_js(ctx, "uncaughtException", &value, None)
    {
        return;
    }
    let err = error::from_js(err, ctx);
    report(ctx, "Uncaught exception", &err);
}

pub(crate) fn report(ctx: &mut Context, kind: &str, err: &Error) {
    let st = state(ctx);
    if let Some(cb) = &st.shared.on_uncaught {
        cb(kind, err);
        return;
    }
    let text = match err {
        Error::Js(e) => e.stack.clone().unwrap_or_else(|| e.to_string()),
        other => other.to_string(),
    };
    st.shared.write_stderr(format!("{kind}: {text}\n").as_bytes());
}

// ---------------------------------------------------------------------------
// Async context and timezone
// ---------------------------------------------------------------------------

/// A `TZ` value as a timezone, as the C library reads it: an IANA name (with an optional leading
/// `:`) or a POSIX rule; anything else, including the empty string, is UTC.
pub(crate) fn timezone_named(tz: &str) -> jiff::tz::TimeZone {
    let name = tz.strip_prefix(':').unwrap_or(tz);
    if name.is_empty() {
        return jiff::tz::TimeZone::UTC;
    }
    jiff::tz::TimeZone::get(name)
        .or_else(|_| jiff::tz::TimeZone::posix(name))
        .unwrap_or(jiff::tz::TimeZone::UTC)
}

/// The async context jobs and timers carry: the AsyncLocalStorage frame (a JS value, `undefined`
/// for the empty frame) and the async_hooks execution/trigger ids. It is captured when a promise
/// reaction or a timer is created and restored while its callback runs (like V8's continuation
/// preserved embedder data / Node's AsyncContextFrame).
#[derive(Clone)]
pub(crate) struct AsyncCtx {
    pub(crate) frame: JsValue,
    pub(crate) id: f64,
    pub(crate) trigger: f64,
}

impl AsyncCtx {
    fn root() -> Self {
        AsyncCtx { frame: JsValue::undefined(), id: 1.0, trigger: 0.0 }
    }
}

impl JsData for AsyncCtx {
    fn trace(&self, tracer: &Tracer) {
        tracer.mark(&self.frame);
    }
}

impl State {
    /// A new async resource triggered by the current execution: captures the current frame.
    pub(crate) fn new_async_ctx(&self) -> AsyncCtx {
        let id = self.next_async_id.get();
        self.next_async_id.set(id + 1.0);
        let current = self.async_ctx.borrow();
        AsyncCtx { frame: current.frame.clone(), id, trigger: current.id }
    }

    pub(crate) fn swap_async_ctx(&self, next: AsyncCtx) -> AsyncCtx {
        std::mem::replace(&mut *self.async_ctx.borrow_mut(), next)
    }
}

/// Call the enabled async hooks (`internal.asyncHookEmit(kind, id, type, trigger)`), never
/// re-entrantly.
pub(crate) fn emit_async_hook(ctx: &mut Context, kind: &str, id: f64, ty: &str, trigger: f64) {
    let st = state(ctx);
    let Some(f) = st.async_hook.borrow().clone() else { return };
    if st.in_async_hook.replace(true) {
        return;
    }
    let args = [js_str(kind), JsValue::from(id), js_str(ty), JsValue::from(trigger)];
    let r = f.call(&JsValue::undefined(), &args, ctx);
    st.in_async_hook.set(false);
    if let Err(e) = r {
        report_uncaught(ctx, e);
    }
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

pub(crate) enum TimerCallback {
    Js(JsFunction, AsyncCtx),
}

/// Where a timer is queued: the real clock, or the fake clock of [`FakeClock`] (microseconds).
#[derive(Clone, Copy)]
enum TimerKey {
    Real((Instant, u64)),
    Fake((u64, u64)),
}

struct TimerEntry {
    callback: TimerCallback,
    interval: Option<Duration>,
    key: TimerKey,
    refed: bool,
}

/// A frozen timer clock (`__jsrtTimers.freeze`, used by test runners' fake timers): timers
/// created while it is installed wait on it and fire only when it is advanced. They never keep
/// the event loop alive, and they are dropped when the clock is thawed. Timers created before
/// keep running on the real clock.
struct FakeClock {
    now_us: u64,
    queue: BTreeMap<(u64, u64), u64>,
}

#[derive(Default)]
pub(crate) struct Timers {
    queue: BTreeMap<(Instant, u64), u64>,
    entries: HashMap<u64, TimerEntry>,
    seq: u64,
    fake: Option<FakeClock>,
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

impl Timers {
    pub(crate) fn add(&mut self, id: u64, callback: TimerCallback, delay: Duration, interval: Option<Duration>) {
        let key = self.key_after(delay, true, id);
        self.entries.insert(id, TimerEntry { callback, interval, key, refed: true });
    }

    /// Add a timer on the real clock even while a fake clock is installed.
    fn add_real(&mut self, id: u64, callback: TimerCallback, delay: Duration) {
        let key = self.key_after(delay, false, id);
        self.entries.insert(id, TimerEntry { callback, interval: None, key, refed: true });
    }

    /// Queue timer `id` `delay` from now on the fake clock (when installed and `fake`) or the
    /// real one.
    fn key_after(&mut self, delay: Duration, fake: bool, id: u64) -> TimerKey {
        self.seq += 1;
        let seq = self.seq;
        match self.fake.as_mut().filter(|_| fake) {
            Some(clock) => {
                let key = (clock.now_us.saturating_add(micros(delay)), seq);
                clock.queue.insert(key, id);
                TimerKey::Fake(key)
            }
            None => {
                let key = (Instant::now() + delay, seq);
                self.queue.insert(key, id);
                TimerKey::Real(key)
            }
        }
    }

    fn unqueue(&mut self, key: TimerKey) {
        match key {
            TimerKey::Real(k) => {
                self.queue.remove(&k);
            }
            TimerKey::Fake(k) => {
                if let Some(clock) = self.fake.as_mut() {
                    clock.queue.remove(&k);
                }
            }
        }
    }

    /// `timeout.ref()` / `unref()`: an unreferenced timer does not keep the loop alive.
    pub(crate) fn set_ref(&mut self, id: u64, refed: bool) {
        if let Some(e) = self.entries.get_mut(&id) {
            e.refed = refed;
        }
    }

    fn refed(&self) -> usize {
        self.entries.values().filter(|e| e.refed && matches!(e.key, TimerKey::Real(_))).count()
    }

    pub(crate) fn cancel(&mut self, id: u64) -> Option<TimerCallback> {
        let e = self.entries.remove(&id)?;
        self.unqueue(e.key);
        Some(e.callback)
    }

    pub(crate) fn refresh(&mut self, id: u64, delay: Duration) {
        let Some(old) = self.entries.get(&id).map(|e| e.key) else { return };
        self.unqueue(old);
        let key = self.key_after(delay, matches!(old, TimerKey::Fake(_)), id);
        if let Some(e) = self.entries.get_mut(&id) {
            e.key = key;
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.queue.keys().next().map(|k| k.0)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Install a fresh fake clock at `now_us`, dropping the timers of a previous one.
    fn freeze(&mut self, now_us: u64) -> Vec<TimerCallback> {
        let dropped = self.thaw();
        self.fake = Some(FakeClock { now_us, queue: BTreeMap::new() });
        dropped
    }

    /// Remove the fake clock and its timers (returned so they are dropped outside the borrow).
    fn thaw(&mut self) -> Vec<TimerCallback> {
        let Some(clock) = self.fake.take() else { return Vec::new() };
        clock.queue.values().filter_map(|id| self.entries.remove(id)).map(|e| e.callback).collect()
    }

    fn fake_count(&self) -> usize {
        self.fake.as_ref().map_or(0, |c| c.queue.len())
    }

    /// Pop the earliest fake timer due at or before `limit_us`, moving the clock to its deadline.
    fn pop_fake(&mut self, limit_us: u64) -> Option<Due> {
        let clock = self.fake.as_mut()?;
        let (&key, &id) = clock.queue.iter().next()?;
        if key.0 > limit_us {
            return None;
        }
        clock.queue.remove(&key);
        clock.now_us = clock.now_us.max(key.0);
        let now_us = clock.now_us;
        let mut entry = self.entries.remove(&id)?;
        match entry.interval {
            Some(interval) => {
                let TimerCallback::Js(f, a) = &entry.callback;
                let cb = Some(Due::Js((f.clone(), a.clone(), false)));
                self.seq += 1;
                let next = (now_us.saturating_add(micros(interval)), self.seq);
                if let Some(clock) = self.fake.as_mut() {
                    clock.queue.insert(next, id);
                }
                entry.key = TimerKey::Fake(next);
                self.entries.insert(id, entry);
                cb
            }
            None => {
                let TimerCallback::Js(f, a) = entry.callback;
                Some(Due::Js((f, a, true)))
            }
        }
    }
}

fn run_due_timers(ctx: &mut Context) {
    let st = state(ctx);
    let now = Instant::now();
    loop {
        let due = {
            let mut timers = st.timers.borrow_mut();
            let Some((&key, &id)) = timers.queue.iter().next() else { break };
            if key.0 > now {
                break;
            }
            timers.queue.remove(&key);
            let Some(mut entry) = timers.entries.remove(&id) else { continue };
            match entry.interval {
                Some(interval) => {
                    let TimerCallback::Js(f, a) = &entry.callback;
                    let cb = Some((f.clone(), a.clone(), false));
                    timers.seq += 1;
                    let next = (now + interval, timers.seq);
                    entry.key = TimerKey::Real(next);
                    timers.queue.insert(next, id);
                    timers.entries.insert(id, entry);
                    cb.map(Due::Js)
                }
                None => {
                    let TimerCallback::Js(f, a) = entry.callback;
                    Some(Due::Js((f, a, true)))
                }
            }
        };
        with_budget(ctx, |ctx| {
            if let Some(due) = due {
                fire(ctx, due);
            }
            run_microtasks(ctx);
        });
    }
}

/// Run one due timer callback in its async context.
fn fire(ctx: &mut Context, due: Due) {
    let st = state(ctx);
    match due {
        Due::Js((f, async_ctx, last)) => {
            let id = async_ctx.id;
            let hooked = st.async_hook.borrow().is_some();
            let previous = st.swap_async_ctx(async_ctx);
            if hooked {
                emit_async_hook(ctx, "before", id, "", 0.0);
            }
            if let Err(e) = f.call(&JsValue::undefined(), &[], ctx) {
                report_uncaught(ctx, e);
            }
            if hooked {
                emit_async_hook(ctx, "after", id, "", 0.0);
                if last {
                    emit_async_hook(ctx, "destroy", id, "", 0.0);
                }
            }
            st.swap_async_ctx(previous);
        }
    }
}

// ---------------------------------------------------------------------------
// Test controls: `gc()` and the fake timer clock (`__jsrtTimers`)
// ---------------------------------------------------------------------------

fn ms_to_us(ms: f64) -> u64 {
    if ms.is_finite() && ms > 0.0 { (ms * 1000.0) as u64 } else { 0 }
}

/// `gc()`: a full collection (WeakRef targets kept alive by the current job are released too).
fn op_gc(_: &JsValue, _: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    crate::js::run_gc();
    Ok(JsValue::undefined())
}

/// `__jsrtTimers.freeze(nowMs)`: install a fake clock; later timers wait on it.
fn op_fake_freeze(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let now = ms_to_us(arg_f64(args, 0, ctx)?);
    let dropped = state(ctx).timers.borrow_mut().freeze(now);
    drop(dropped);
    Ok(JsValue::undefined())
}

/// `__jsrtTimers.thaw()`: remove the fake clock and drop its pending timers.
fn op_fake_thaw(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let dropped = state(ctx).timers.borrow_mut().thaw();
    drop(dropped);
    Ok(JsValue::undefined())
}

/// `__jsrtTimers.frozen()`
fn op_fake_frozen(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(state(ctx).timers.borrow().fake.is_some()))
}

/// `__jsrtTimers.now()`: the fake clock in ms (`undefined` when not frozen).
fn op_fake_now(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(state(ctx).timers.borrow().fake.as_ref().map_or(JsValue::undefined(), |c| JsValue::from(c.now_us as f64 / 1000.0)))
}

/// `__jsrtTimers.next()`: deadline (ms) of the earliest fake timer, or `undefined`.
fn op_fake_next(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let timers = state(ctx).timers.borrow().fake.as_ref().and_then(|c| c.queue.keys().next().map(|k| k.0));
    Ok(timers.map_or(JsValue::undefined(), |us| JsValue::from(us as f64 / 1000.0)))
}

/// `__jsrtTimers.count()`: pending fake timers.
fn op_fake_count(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(state(ctx).timers.borrow().fake_count() as f64))
}

/// `__jsrtTimers.runNext(limitMs = Infinity)`: fire the earliest fake timer due by `limitMs`,
/// moving the clock to its deadline. Promise jobs are not run (as with fake timers in Node).
/// Returns whether a timer fired.
fn op_fake_run_next(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let limit = args.get_or_undefined(0);
    let limit = if limit.is_undefined() { u64::MAX } else { ms_to_us(limit.to_number(ctx)?) };
    let due = state(ctx).timers.borrow_mut().pop_fake(limit);
    let fired = due.is_some();
    if let Some(due) = due {
        fire(ctx, due);
    }
    Ok(JsValue::from(fired))
}

/// `__jsrtTimers.advance(ms)`: fire every fake timer due within `ms`, then move the clock by `ms`.
fn op_fake_advance(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let delta = ms_to_us(arg_f64(args, 0, ctx)?);
    let st = state(ctx);
    let Some(target) = st.timers.borrow().fake.as_ref().map(|c| c.now_us.saturating_add(delta)) else {
        return Ok(JsValue::from(0));
    };
    let mut fired = 0u32;
    loop {
        let due = st.timers.borrow_mut().pop_fake(target);
        let Some(due) = due else { break };
        fire(ctx, due);
        fired += 1;
    }
    if let Some(clock) = st.timers.borrow_mut().fake.as_mut() {
        clock.now_us = clock.now_us.max(target);
    }
    Ok(JsValue::from(fired))
}

/// `__jsrtTimers.setNow(ms)`: move the fake clock without firing timers.
fn op_fake_set_now(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let now = ms_to_us(arg_f64(args, 0, ctx)?);
    if let Some(clock) = state(ctx).timers.borrow_mut().fake.as_mut() {
        clock.now_us = now;
    }
    Ok(JsValue::undefined())
}

/// `__jsrtTimers.realTimeout(callback, ms) -> id`: a one-shot timer on the real clock even while
/// frozen (a test runner's own deadlines and event-loop yields). Cancel with `clearTimeout(id)`.
fn op_real_timeout(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(f) = args.get_or_undefined(0).as_function() else {
        return Err(type_error("The \"callback\" argument must be of type function"));
    };
    let delay = Duration::from_micros(ms_to_us(arg_f64(args, 1, ctx)?));
    let st = state(ctx);
    let id = st.next_id();
    let async_ctx = st.new_async_ctx();
    st.timers.borrow_mut().add_real(id, TimerCallback::Js(f, async_ctx), delay);
    Ok(JsValue::from(id as f64))
}

/// `__jsrtTimers.unref(id)`: the timer no longer keeps the event loop alive.
fn op_real_unref(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    state(ctx).timers.borrow_mut().set_ref(id, false);
    Ok(JsValue::undefined())
}

const TIMER_CONTROL_OPS: &[(&str, usize, OpFn)] = &[
    ("freeze", 1, op_fake_freeze),
    ("thaw", 0, op_fake_thaw),
    ("frozen", 0, op_fake_frozen),
    ("now", 0, op_fake_now),
    ("setNow", 1, op_fake_set_now),
    ("next", 0, op_fake_next),
    ("count", 0, op_fake_count),
    ("runNext", 1, op_fake_run_next),
    ("advance", 1, op_fake_advance),
    ("realTimeout", 2, op_real_timeout),
    ("unref", 1, op_real_unref),
];

/// Install the `gc()` global and the `__jsrtTimers` object when enabled by the options.
pub(crate) fn install_controls(ctx: &mut Context) -> JsResult<()> {
    let st = state(ctx);
    let global = ctx.global_object();
    if st.shared.options.expose_gc {
        let f = make_fn(ctx, "gc", 0, op_gc);
        global.set(js_string!("gc"), f, true, ctx)?;
    }
    if st.shared.options.timer_control {
        let obj = JsObject::with_object_proto(ctx.intrinsics());
        define_ops(ctx, &obj, TIMER_CONTROL_OPS)?;
        global.set(js_string!("__jsrtTimers"), obj, true, ctx)?;
    }
    Ok(())
}

enum Due {
    Js((JsFunction, AsyncCtx, bool)),
}

// ---------------------------------------------------------------------------
// Async ops: Rust futures whose results resolve JS promises
// ---------------------------------------------------------------------------

/// A boxed conversion run on the JS thread once an async op has completed.
pub(crate) type Finish = Box<dyn FnOnce(&mut Context) -> JsResult<JsValue> + Send>;

/// Spawn `fut` on tokio and return a promise that settles with the value produced
/// by the returned [`Finish`] closure (run on the JS thread).
pub(crate) fn spawn_promise<F>(ctx: &mut Context, fut: F) -> JsValue
where
    F: Future<Output = Finish> + Send + 'static,
{
    let (promise, resolvers) = JsPromise::new_pending(ctx);
    let st = state(ctx);
    let id = st.next_id();
    st.pending.borrow_mut().insert(id, resolvers);
    let poster = st.poster();
    st.tokio.spawn(async move {
        let finish = fut.await;
        poster.post(move |ctx| {
            let result = finish(ctx);
            settle_pending(ctx, id, result);
        });
    });
    promise.into()
}

/// Spawn a future producing `Result<T, Error>`; `convert` turns `T` into a JS value.
pub(crate) fn spawn_result<T, F, C>(ctx: &mut Context, fut: F, convert: C) -> JsValue
where
    T: Send + 'static,
    F: Future<Output = Result<T, Error>> + Send + 'static,
    C: FnOnce(T, &mut Context) -> JsResult<JsValue> + Send + 'static,
{
    spawn_promise(ctx, async move {
        let r = fut.await;
        Box::new(move |ctx: &mut Context| match r {
            Ok(v) => convert(v, ctx),
            Err(e) => Err(error::to_js(&e, ctx)),
        }) as Finish
    })
}

fn settle_pending(ctx: &mut Context, id: u64, result: JsResult<JsValue>) {
    let st = state(ctx);
    let Some(resolvers) = st.pending.borrow_mut().remove(&id) else { return };
    let r = match result {
        Ok(v) => resolvers.resolve.call(&JsValue::undefined(), &[v], ctx),
        Err(e) => {
            let v = e.into_opaque(ctx).unwrap_or_else(|e| JsValue::from(js_string!(e.to_string().as_str())));
            resolvers.reject.call(&JsValue::undefined(), &[v], ctx)
        }
    };
    if let Err(e) = r {
        report_uncaught(ctx, e);
    }
}

/// Wait for `value` (awaiting it if it is a promise) and invoke `done` with the outcome.
pub(crate) fn when_settled(ctx: &mut Context, value: JsResult<JsValue>, done: impl FnOnce(&mut Context, Result<JsValue, JsValue>) + 'static) {
    let value = match value {
        Ok(v) => v,
        Err(e) => {
            let v = e.into_opaque(ctx).unwrap_or_else(|e| JsValue::from(js_string!(e.to_string().as_str())));
            done(ctx, Err(v));
            return;
        }
    };
    let Some(promise) = value.as_promise() else {
        done(ctx, Ok(value));
        return;
    };
    type Done = Box<dyn FnOnce(&mut Context, Result<JsValue, JsValue>)>;
    let slot: Rc<RefCell<Option<Done>>> = Rc::new(RefCell::new(Some(Box::new(done))));
    let s1 = slot.clone();
    let s2 = slot;
    // SAFETY: the closures capture only an `Rc<RefCell<..>>` holding a Rust closure
    // that does not own GC-traced values (callers pass reply channels / ids only).
    let on_ok = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let taken = s1.borrow_mut().take();
            if let Some(f) = taken {
                f(ctx, Ok(args.get_or_undefined(0).clone()));
            }
            Ok(JsValue::undefined())
        })
    };
    let on_err = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let taken = s2.borrow_mut().take();
            if let Some(f) = taken {
                f(ctx, Err(args.get_or_undefined(0).clone()));
            }
            Ok(JsValue::undefined())
        })
    };
    let realm = ctx.realm().clone();
    let on_ok = FunctionObjectBuilder::new(&realm, on_ok).build();
    let on_err = FunctionObjectBuilder::new(&realm, on_err).build();
    if let Err(e) = promise.then(Some(on_ok), Some(on_err), ctx) {
        report_uncaught(ctx, e);
    }
}

// ---------------------------------------------------------------------------
// Stack remapping
// ---------------------------------------------------------------------------

/// Rewrite `file:line:col` positions in a stack trace using transpiler source maps.
pub(crate) fn remap_stack(ctx: &Context, stack: &str) -> String {
    let Some(st) = ctx.get_data::<Rc<State>>() else { return stack.to_string() };
    let maps = st.source_maps.borrow();
    if maps.is_empty() {
        return stack.to_string();
    }
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"([^\s()]+):(\d+):(\d+)").unwrap());
    re.replace_all(stack, |c: &regex::Captures<'_>| {
        let file = &c[1];
        let line: u32 = c[2].parse().unwrap_or(0);
        let col: u32 = c[3].parse().unwrap_or(0);
        match maps.get(file).and_then(|m| m.lookup(line, col)) {
            Some((l, cc)) => format!("{file}:{l}:{cc}"),
            None => c[0].to_string(),
        }
    })
    .into_owned()
}

// ---------------------------------------------------------------------------
// Native function helpers
// ---------------------------------------------------------------------------

/// Signature of the internal ops.
pub(crate) type OpFn = fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>;

/// Create a JS function from a plain function pointer.
pub(crate) fn make_fn(ctx: &mut Context, name: &str, length: usize, f: OpFn) -> JsFunction {
    let realm = ctx.realm().clone();
    FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f))
        .name(JsString::from(name))
        .length(length)
        .build()
}

/// Define a set of ops on `target`.
pub(crate) fn define_ops(ctx: &mut Context, target: &JsObject, ops: &[(&str, usize, OpFn)]) -> JsResult<()> {
    for (name, len, f) in ops {
        let func = make_fn(ctx, name, *len, *f);
        target.set(JsString::from(*name), func, true, ctx)?;
    }
    Ok(())
}

/// Throw a TypeError.
pub(crate) fn type_error(msg: impl Into<String>) -> crate::js::JsError {
    JsNativeError::typ().with_message(msg.into()).into()
}

/// Argument helpers.
pub(crate) fn arg_string(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<String> {
    Ok(args.get_or_undefined(i).to_string(ctx)?.to_std_string_lossy())
}

pub(crate) fn arg_opt_string(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<Option<String>> {
    let v = args.get_or_undefined(i);
    if v.is_null_or_undefined() {
        Ok(None)
    } else {
        Ok(Some(v.to_string(ctx)?.to_std_string_lossy()))
    }
}

pub(crate) fn arg_f64(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<f64> {
    args.get_or_undefined(i).to_number(ctx)
}

pub(crate) fn js_str(s: &str) -> JsValue {
    JsValue::from(JsString::from(s))
}

// ---------------------------------------------------------------------------
// Core ops (handles, timers, output)
// ---------------------------------------------------------------------------

fn op_handle_new(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = state(ctx).new_handle(args.get_or_undefined(0).clone());
    Ok(JsValue::from(id as f64))
}

fn op_handle_get(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    state(ctx).get_handle(id).ok_or_else(|| type_error(format!("invalid JS handle {id}")))
}

fn op_host_fn_get(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    state(ctx)
        .host_fns
        .borrow()
        .get(&id)
        .cloned()
        .map(JsValue::from)
        .ok_or_else(|| type_error(format!("invalid host function {id}")))
}

fn op_timer_start(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(f) = args.get_or_undefined(0).as_function() else {
        return Err(type_error("The \"callback\" argument must be of type function"));
    };
    let ms = args.get_or_undefined(1).to_number(ctx)?;
    let ms = if ms.is_finite() && ms > 0.0 { ms } else { 0.0 };
    let repeat = args.get_or_undefined(2).to_boolean();
    let kind = args.get_or_undefined(3);
    let kind = if kind.is_undefined() { "Timeout".to_string() } else { kind.to_string(ctx)?.to_std_string_lossy() };
    let delay = Duration::from_micros((ms * 1000.0) as u64);
    let st = state(ctx);
    let id = st.next_id();
    let async_ctx = st.new_async_ctx();
    if st.async_hook.borrow().is_some() {
        emit_async_hook(ctx, "init", async_ctx.id, &kind, async_ctx.trigger);
    }
    st.timers.borrow_mut().add(id, TimerCallback::Js(f, async_ctx), delay, repeat.then_some(delay.max(Duration::from_millis(1))));
    Ok(JsValue::from(id as f64))
}

fn op_timer_clear(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)?;
    if id.is_finite() && id >= 0.0 {
        let st = state(ctx);
        let removed = st.timers.borrow_mut().cancel(id as u64);
        if let Some(TimerCallback::Js(_, a)) = removed
            && st.async_hook.borrow().is_some()
        {
            emit_async_hook(ctx, "destroy", a.id, "", 0.0);
        }
    }
    Ok(JsValue::undefined())
}

/// `ops.asyncContext() -> [frame, executionAsyncId, triggerAsyncId]`
fn op_async_context(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let a = state(ctx).async_ctx.borrow().clone();
    Ok(crate::js::JsArray::from_iter([a.frame, JsValue::from(a.id), JsValue::from(a.trigger)], ctx).into())
}

/// `ops.asyncFrameSet(frame)`: AsyncLocalStorage run/enterWith/exit.
fn op_async_frame_set(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    state(ctx).async_ctx.borrow_mut().frame = args.get_or_undefined(0).clone();
    Ok(JsValue::undefined())
}

/// `ops.asyncResourceNew(type) -> context`: a new async resource (AsyncResource) capturing the
/// current frame, triggered by the current execution; emits `init` to enabled hooks.
fn op_async_resource_new(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ty = arg_string(args, 0, ctx)?;
    let trigger = args.get_or_undefined(1);
    let st = state(ctx);
    let mut a = st.new_async_ctx();
    if !trigger.is_undefined() {
        a.trigger = trigger.to_number(ctx)?;
    }
    if st.async_hook.borrow().is_some() {
        emit_async_hook(ctx, "init", a.id, &ty, a.trigger);
    }
    let proto = ctx.intrinsics().constructors().object().prototype();
    Ok(JsObject::from_proto_and_data(Some(proto), a).into())
}

/// `ops.asyncEnter(context | undefined) -> previous context`: run inside a resource's context.
fn op_async_enter(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let st = state(ctx);
    let next = match args.get_or_undefined(0).as_object().and_then(|o| o.downcast_ref::<AsyncCtx>().map(|a| a.clone())) {
        Some(a) => a,
        None => AsyncCtx::root(),
    };
    let previous = st.swap_async_ctx(next);
    let proto = ctx.intrinsics().constructors().object().prototype();
    Ok(JsObject::from_proto_and_data(Some(proto), previous).into())
}

/// `ops.asyncResourceInfo(context) -> [asyncId, triggerAsyncId]`
fn op_async_resource_info(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let a = args.get_or_undefined(0).as_object().and_then(|o| o.downcast_ref::<AsyncCtx>().map(|a| a.clone())).unwrap_or_else(AsyncCtx::root);
    Ok(crate::js::JsArray::from_iter([JsValue::from(a.id), JsValue::from(a.trigger)], ctx).into())
}

/// `ops.asyncHooksEnable(emit | null)`: route hook events to `emit(kind, id, type, trigger)`.
fn op_async_hooks_enable(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    *state(ctx).async_hook.borrow_mut() = args.get_or_undefined(0).as_object();
    Ok(JsValue::undefined())
}

fn op_timer_ref(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let refed = args.get_or_undefined(1).to_boolean();
    state(ctx).timers.borrow_mut().set_ref(id, refed);
    Ok(JsValue::undefined())
}

/// `ops.loopRef(delta)`: count a handle that keeps the event loop alive (+1) or release it (-1).
fn op_loop_ref(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let delta = arg_f64(args, 0, ctx)? as i64;
    state(ctx).loop_ref(delta);
    Ok(JsValue::undefined())
}

/// `ops.loopAlive()`: whether the event loop has referenced work.
fn op_loop_alive(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(state(ctx).alive()))
}

fn op_timer_refresh(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = arg_f64(args, 0, ctx)? as u64;
    let ms = arg_f64(args, 1, ctx)?.max(0.0);
    state(ctx).timers.borrow_mut().refresh(id, Duration::from_micros((ms * 1000.0) as u64));
    Ok(JsValue::undefined())
}

fn op_write(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let fd = arg_f64(args, 0, ctx)? as i32;
    let data = crate::ops::bytes_arg(args.get_or_undefined(1), ctx)?;
    let st = state(ctx);
    if fd == 2 {
        st.shared.write_stderr(&data);
    } else {
        st.shared.write_stdout(&data);
    }
    Ok(JsValue::undefined())
}

fn op_report_error(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let kind = arg_string(args, 0, ctx)?;
    if kind == "Uncaught exception" && handled_by_js(ctx, "uncaughtException", args.get_or_undefined(1), None) {
        return Ok(JsValue::undefined());
    }
    let err = error::from_thrown(args.get_or_undefined(1), ctx);
    report(ctx, &kind, &err);
    Ok(JsValue::undefined())
}

fn op_remap_stack(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = arg_string(args, 0, ctx)?;
    Ok(js_str(&remap_stack(ctx, &s)))
}

fn op_now(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let st = state(ctx);
    Ok(JsValue::from(st.shared.started.elapsed().as_secs_f64() * 1000.0))
}

fn op_hrtime(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let st = state(ctx);
    Ok(JsValue::from(st.shared.started.elapsed().as_nanos() as f64))
}

fn op_time_origin(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let st = state(ctx);
    Ok(JsValue::from(st.shared.time_origin_ms))
}

pub(crate) const CORE_OPS: &[(&str, usize, OpFn)] = &[
    ("handleNew", 1, op_handle_new),
    ("handleGet", 1, op_handle_get),
    ("hostFnGet", 1, op_host_fn_get),
    ("timerStart", 4, op_timer_start),
    ("timerClear", 1, op_timer_clear),
    ("timerRefresh", 2, op_timer_refresh),
    ("timerRef", 2, op_timer_ref),
    ("asyncContext", 0, op_async_context),
    ("ticksScheduled", 0, op_ticks_scheduled),
    ("loopTiming", 0, op_loop_timing),
    ("asyncFrameSet", 1, op_async_frame_set),
    ("asyncResourceNew", 2, op_async_resource_new),
    ("asyncEnter", 1, op_async_enter),
    ("asyncResourceInfo", 1, op_async_resource_info),
    ("asyncHooksEnable", 1, op_async_hooks_enable),
    ("loopRef", 1, op_loop_ref),
    ("loopAlive", 0, op_loop_alive),
    ("write", 2, op_write),
    ("reportError", 2, op_report_error),
    ("remapStack", 1, op_remap_stack),
    ("now", 0, op_now),
    ("hrtime", 0, op_hrtime),
    ("timeOrigin", 0, op_time_origin),
];

// ---------------------------------------------------------------------------
// Thread bootstrap and event loop
// ---------------------------------------------------------------------------

/// Evaluate a glue script of the form `(function (ops, internal) { ... })`.
pub(crate) fn run_glue(ctx: &mut Context, name: &str, src: &str, ops: &JsObject, internal: &JsObject) -> JsResult<()> {
    let wrapped = format!("(function (ops, internal) {{\"use strict\";\n{src}\n}})");
    let path = PathBuf::from(format!("pi-jsrt:{name}"));
    let f = ctx.eval(Source::from_bytes(wrapped.as_bytes()).with_path(&path))?;
    let f = f.as_function().ok_or_else(|| type_error("glue did not evaluate to a function"))?;
    f.call(&JsValue::undefined(), &[ops.clone().into(), internal.clone().into()], ctx)?;
    Ok(())
}

pub(crate) struct ThreadInit {
    pub(crate) rt: Weak<Inner>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) tokio: tokio::runtime::Handle,
    pub(crate) tx: mpsc::Sender<Msg>,
    pub(crate) rx: mpsc::Receiver<Msg>,
    pub(crate) cwd: PathBuf,
}

/// Build the context and run the event loop until shutdown.
pub(crate) fn thread_main(init: ThreadInit, ready: std::sync::mpsc::SyncSender<Result<(), Error>>) {
    let ThreadInit { rt, shared, tokio, tx, rx, cwd } = init;
    // QuickJS checks the native stack: leave room below the thread's stack for Rust frames.
    let max_stack = shared.options.thread_stack_size.saturating_sub(shared.options.thread_stack_size / 4).max(256 * 1024);
    let engine = match Engine::new(EngineOptions { max_stack_size: max_stack, memory_limit: 0 }) {
        Ok(e) => e,
        Err(e) => {
            let _ = ready.send(Err(Error::host(format!("failed to create JS context: {e}"))));
            return;
        }
    };
    let mut ctx = engine.context();
    let loader = Rc::new(crate::loader::Loader::new(shared.clone()));
    crate::loader::install(&engine, loader.clone());
    // `process.env.TZ` is the runtime's timezone, as in Node (see `ops.setEnv`).
    let timezone = Rc::new(RefCell::new(shared.env.get("TZ").map(|tz| timezone_named(tz))));
    let st = Rc::new(State {
        tx,
        rt,
        shared: shared.clone(),
        tokio,
        handles: RefCell::default(),
        created: RefCell::default(),
        host_fns: RefCell::default(),
        timers: RefCell::default(),
        pending: RefCell::default(),
        source_maps: RefCell::default(),
        rejections: RefCell::default(),
        internal: RefCell::default(),
        ops: RefCell::default(),
        cwd: RefCell::new(cwd),
        stopping: Cell::new(false),
        loop_refs: Cell::new(0),
        queued: Arc::default(),
        async_ctx: RefCell::new(AsyncCtx::root()),
        next_async_id: Cell::new(2.0),
        async_hook: RefCell::default(),
        in_async_hook: Cell::new(false),
        ticks_pending: Cell::new(false),
        timezone,
        loop_start_ms: Cell::new(0.0),
        idle_ms: Cell::new(0.0),
        idle_waiters: RefCell::default(),
        budget: Cell::new(None),
        job_stack: RefCell::default(),
    });
    ctx.insert_data(st.clone());
    TIMEZONE.with(|t| *t.borrow_mut() = Some(st.timezone.clone()));
    install_hooks(&engine, &st);

    if let Err(e) = crate::ops::bootstrap(&mut ctx) {
        let err = error::from_js(e, &mut ctx);
        let _ = ready.send(Err(err));
        return;
    }
    run_microtasks(&mut ctx);
    let _ = ready.send(Ok(()));

    event_loop(&mut ctx, &rx);

    // Tear down: drop every JS value we hold before the engine goes away.
    st.stopping.set(true);
    st.handles.borrow_mut().clear();
    st.host_fns.borrow_mut().clear();
    *st.timers.borrow_mut() = Timers::default();
    st.pending.borrow_mut().clear();
    st.rejections.borrow_mut().clear();
    *st.internal.borrow_mut() = None;
    *st.ops.borrow_mut() = None;
    *st.async_ctx.borrow_mut() = AsyncCtx::root();
    *st.async_hook.borrow_mut() = None;
    loader.clear();
    crate::ops::teardown(&mut ctx);
    st.job_stack.borrow_mut().clear();
    TIMEZONE.with(|t| t.borrow_mut().take());
    drop(st);
    drop(loader);
    drop(engine);
}

/// Rejection tracking, the interrupt handler (runaway-script budget), promise job async contexts
/// and the timezone of `Date`.
fn install_hooks(engine: &Engine, st: &Rc<State>) {
    unsafe {
        crate::js::ext::JS_SetJobContextHooks(engine.raw_runtime(), Some(job_capture), Some(job_enter), Some(job_exit), std::ptr::null_mut());
        crate::js::ext::JS_SetTimezoneOffsetFunc(engine.raw_runtime(), Some(timezone_offset), std::ptr::null_mut());
    }
    let runtime = engine.runtime();
    runtime.set_host_promise_rejection_tracker(Some(Box::new(|ctx, promise, _reason, is_handled| {
        let ctx = Context::from_raw(ctx.as_raw().as_ptr());
        let Some(st) = ctx.get_data::<Rc<State>>().cloned() else { return };
        // SAFETY: `promise` is a live value of this runtime; we take our own reference.
        let promise = unsafe { JsValue::from_raw_dup(promise.as_raw()) };
        let Some(obj) = promise.as_object() else { return };
        let mut list = st.rejections.borrow_mut();
        if is_handled {
            list.retain(|p| !JsObject::equals(p, &obj));
        } else {
            list.push(obj);
        }
    })));
    if st.shared.options.loop_iteration_limit.is_some() {
        let weak = Rc::downgrade(st);
        runtime.set_interrupt_handler(Some(Box::new(move || {
            let Some(st) = weak.upgrade() else { return false };
            match st.budget.get() {
                Some(0) => true,
                Some(n) => {
                    st.budget.set(Some(n - 1));
                    false
                }
                None => false,
            }
        })));
    }
}

/// A promise reaction is created: capture the current async context (HostMakeJobCallback).
unsafe extern "C" fn job_capture(raw: *mut qjs::JSContext, _opaque: *mut std::ffi::c_void) -> qjs::JSValue {
    let mut ctx = Context::from_raw(raw);
    let Some(st) = ctx.get_data::<Rc<State>>().cloned() else { return JsValue::undefined().into_raw() };
    let async_ctx = st.new_async_ctx();
    if st.async_hook.borrow().is_some() {
        emit_async_hook(&mut ctx, "init", async_ctx.id, "PROMISE", async_ctx.trigger);
    }
    JsValue::from(JsObject::from_proto_and_data(None, async_ctx)).into_raw()
}

/// A promise job runs: enter the context it captured (HostCallJobCallback).
unsafe extern "C" fn job_enter(raw: *mut qjs::JSContext, job_context: qjs::JSValue, _opaque: *mut std::ffi::c_void) -> qjs::JSValue {
    let mut ctx = Context::from_raw(raw);
    let Some(st) = ctx.get_data::<Rc<State>>().cloned() else { return JsValue::undefined().into_raw() };
    // SAFETY: a live value of this runtime, borrowed for the call.
    let data = unsafe { JsValue::from_raw_dup(job_context) };
    let saved = data.as_object().and_then(|o| o.downcast_ref::<AsyncCtx>().map(|a| a.clone()));
    let Some(saved) = saved else {
        st.job_stack.borrow_mut().push((st.async_ctx.borrow().clone(), false));
        return JsValue::undefined().into_raw();
    };
    let id = saved.id;
    let hooked = st.async_hook.borrow().is_some();
    let previous = st.swap_async_ctx(saved);
    st.job_stack.borrow_mut().push((previous, hooked));
    if hooked {
        emit_async_hook(&mut ctx, "before", id, "", 0.0);
    }
    JsValue::from(id).into_raw()
}

/// A promise job finished: restore the context saved by [`job_enter`].
unsafe extern "C" fn job_exit(raw: *mut qjs::JSContext, previous: qjs::JSValue, _opaque: *mut std::ffi::c_void) {
    let mut ctx = Context::from_raw(raw);
    // SAFETY: we own the reference returned by `job_enter`.
    let id = unsafe { JsValue::from_raw(previous) };
    let Some(st) = ctx.get_data::<Rc<State>>().cloned() else { return };
    let Some((saved, hooked)) = st.job_stack.borrow_mut().pop() else { return };
    if hooked && let Some(id) = id.as_number() {
        emit_async_hook(&mut ctx, "after", id, "", 0.0);
    }
    st.swap_async_ctx(saved);
}

/// `Date`'s local timezone: the runtime's `TZ`, else the system's.
unsafe extern "C" fn timezone_offset(time_ms: i64, _opaque: *mut std::ffi::c_void) -> i32 {
    let at = jiff::Timestamp::from_millisecond(time_ms).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    let offset = current_timezone().map(|tz| tz.to_offset(at).seconds()).unwrap_or(0);
    -(offset / 60)
}

thread_local! {
    /// The timezone of the runtime on this thread (`None`: the system's).
    static TIMEZONE: RefCell<Option<Rc<RefCell<Option<jiff::tz::TimeZone>>>>> = const { RefCell::new(None) };
}

fn current_timezone() -> Option<jiff::tz::TimeZone> {
    let runtime_tz = TIMEZONE.with(|t| t.borrow().as_ref().and_then(|tz| tz.borrow().clone()));
    runtime_tz.or_else(|| jiff::tz::TimeZone::try_system().ok())
}

/// QuickJS calls the interrupt handler about every 10000 operations: the number of calls a
/// synchronous run may take before it is stopped for `loop_iteration_limit` iterations.
fn interrupt_budget(limit: u64) -> u64 {
    (limit / 10_000).max(1)
}

/// Run `f` with a fresh runaway-script budget (when `loop_iteration_limit` is set).
pub(crate) fn with_budget<R>(ctx: &mut Context, f: impl FnOnce(&mut Context) -> R) -> R {
    let st = state(ctx);
    let Some(limit) = st.shared.options.loop_iteration_limit else { return f(ctx) };
    let previous = st.budget.replace(Some(interrupt_budget(limit)));
    let r = f(ctx);
    st.budget.set(previous);
    r
}

fn event_loop(ctx: &mut Context, rx: &mpsc::Receiver<Msg>) {
    let st = state(ctx);
    st.loop_start_ms.set(st.shared.started.elapsed().as_secs_f64() * 1000.0);
    loop {
        let next = st.timers.borrow().next_deadline();
        let waiting = Instant::now();
        let msg = match next {
            Some(deadline) => {
                let now = Instant::now();
                if deadline <= now {
                    Err(mpsc::RecvTimeoutError::Timeout)
                } else {
                    rx.recv_timeout(deadline - now)
                }
            }
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        st.idle_ms.set(st.idle_ms.get() + waiting.elapsed().as_secs_f64() * 1000.0);
        match msg {
            Ok(Msg::Task(task)) => with_budget(ctx, |ctx| {
                task(ctx);
                run_microtasks(ctx);
            }),
            Ok(Msg::Release(id)) => st.release(id),
            Ok(Msg::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        run_due_timers(ctx);
        st.notify_idle();
    }
}

/// Collect handle ids created while running `f` (used to sweep unadopted handles).
pub(crate) fn collect_created<R>(ctx: &mut Context, f: impl FnOnce(&mut Context) -> R) -> (R, Vec<u64>) {
    let st = state(ctx);
    let prev = st.created.borrow_mut().replace(Vec::new());
    let r = f(ctx);
    let ids = std::mem::replace(&mut *st.created.borrow_mut(), prev).unwrap_or_default();
    if let Some(outer) = st.created.borrow_mut().as_mut() {
        outer.extend(ids.iter().copied());
    }
    (r, ids)
}

/// Number of live timers (for tests / diagnostics).
pub(crate) fn timer_count(ctx: &Context) -> usize {
    state(ctx).timers.borrow().len()
}
