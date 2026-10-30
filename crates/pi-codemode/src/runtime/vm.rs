//! Drives one Boa VM through one script on a dedicated thread, independent of the transport that
//! connects it to the host. The thread launcher and the remote server both run the VM through this
//! module, so the bridge between prelude and host exists once.
//!
//! Contract with the caller:
//! - `post` receives every report for the host, synchronously from the VM thread. It must not
//!   block on the host.
//! - [`VmHandle::deliver`] hands the VM a tool result; the VM applies it and runs the script until
//!   it waits again.
//! - [`VmHandle::stop`] sets an interrupt flag and asks the thread to exit. Boa cannot interrupt
//!   running code from outside, so the flag is polled at every bridge call, on every iteration of
//!   the script's loops (see [`super::instrument`]), and between promise jobs. Each loop iteration
//!   and promise job also counts against the execution's interrupt budget, if it has one. Code the
//!   script generates at run time (`eval`, `Function`) is only bounded by Boa's per-call loop
//!   iteration limit and the recursion limit, which raise errors the script cannot catch. The
//!   thread is never joined: stopping does not wait for it.
//! - The VM never decides on its own when it is finished: after `done` or `crash` it stays alive
//!   until it is stopped. Once it has reported a terminal message, the next bridge call (output,
//!   tool call) ends the script with an uncatchable error instead of letting it run on.
//!
//! Deterministic mode (`data.deterministic`, used by `CodemodeSandbox::start()`): a script must make
//! the same calls with the same arguments every time it runs with the same results, so a host can
//! replay it after a restart. The VM is otherwise deterministic, because it only runs when the host
//! delivers something and has no timers. What remains is fixed here:
//! - Time: `Date` reads virtual time, `startedAt` and then the `now` of the result being processed,
//!   never going backwards; `performance.now()` counts from `startedAt`.
//! - Randomness: `Math.random()` draws from a PRNG over the seed.
//! - Timezone: UTC, not the host's.
//! - Garbage collection: `WeakRef` and `FinalizationRegistry` are not available (nor `Temporal`,
//!   which reads the host's timezone).
//! - Delivery order: results carry a `seq` and are applied strictly in that order, buffering early
//!   ones, because a transport may deliver them in another order than the host sent them.
//!
//! The VM reports `started` with its fingerprint before the script runs, and `idle` with the
//! number of calls issued after each drain, so the host can check a replay step by step.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use boa_engine::context::time::JsInstant;
use boa_engine::context::{Clock, HostHooks};
use boa_engine::job::{Job, JobExecutor};
use boa_engine::object::FunctionObjectBuilder;
use boa_engine::prelude::{EngineError, RuntimeLimitError};
use boa_engine::property::PropertyKey;
use boa_engine::{Context, JsError, JsObject, JsResult, JsString, JsValue, NativeFunction, Source, js_string};

use super::prelude_source::{PRELUDE_HASH, PRELUDE_SOURCE};
use super::protocol::{DoneMessage, HostToWorkerMessage, VmStartData, WorkerToHostMessage};
use crate::types::{CodemodeCallTarget, CodemodeDuring, CodemodeOutputItem};

/// Bumped whenever this module or the prelude changes what a deterministic script observes,
/// beyond what the fingerprint's other parts (engine version, prelude hash, limits) already capture.
const DETERMINISTIC_VERSION: u32 = 5;

/// The engine named in the fingerprint.
const ENGINE: &str = "boa 0.22";

/// Promise jobs run between two interrupt polls while draining the job queue.
const JOBS_PER_POLL: u64 = 50;

/// Loop iterations one function invocation may run, for code the instrumentation does not reach
/// (`eval`, `Function`). Exceeding it ends the script with `kind: "timeout"`.
pub const DEFAULT_LOOP_ITERATION_LIMIT: u64 = 100_000_000;

/// Maximum JS call depth. Deep recursion past it throws a `RangeError` the script can catch, as in QuickJS.
pub const DEFAULT_RECURSION_LIMIT: usize = 4096;

/// Native stack of a VM thread. Large, because native built-ins that call back into JS (for
/// example `Array.prototype.map`) recurse on it.
const VM_THREAD_STACK_SIZE: usize = 64 * 1024 * 1024;

/// What a VM reports to the side that launched it.
#[derive(Debug, Clone, PartialEq)]
pub enum VmReport {
    /// A protocol message for the host.
    Message(WorkerToHostMessage),
    /// The script used up its interrupt budget or loop iteration limit; the launcher reports the
    /// message as a timeout.
    Exhausted(String),
}

/// Limits of one VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmLimits {
    /// Loop iterations and promise jobs the whole execution may run (`None`: unlimited). Part of
    /// the fingerprint.
    pub interrupt_budget: Option<u64>,
    /// Loop iterations one function invocation may run in code the instrumentation does not reach.
    pub loop_iteration_limit: Option<u64>,
    /// Maximum JS call depth.
    pub recursion_limit: usize,
}

impl Default for VmLimits {
    fn default() -> Self {
        VmLimits {
            interrupt_budget: None,
            loop_iteration_limit: Some(DEFAULT_LOOP_ITERATION_LIMIT),
            recursion_limit: DEFAULT_RECURSION_LIMIT,
        }
    }
}

/// Starts a VM.
pub struct StartVmOptions {
    pub data: VmStartData,
    pub limits: VmLimits,
    /// Receives every report, on the VM thread.
    pub post: Arc<dyn Fn(VmReport) + Send + Sync>,
    /// Called on the VM thread right before it exits.
    pub on_exit: Option<Box<dyn FnOnce() + Send>>,
}

enum Command {
    Deliver(HostToWorkerMessage),
    Stop,
}

/// The host's handle on a VM thread.
#[derive(Clone)]
pub struct VmHandle {
    tx: mpsc::Sender<Command>,
    interrupt: Arc<AtomicBool>,
}

impl VmHandle {
    /// Hand the VM a call's result.
    pub fn deliver(&self, message: HostToWorkerMessage) {
        let _ = self.tx.send(Command::Deliver(message));
    }

    /// Ask the VM to stop: interrupt the script at its next poll and let the thread exit. Does not
    /// wait for the thread.
    pub fn stop(&self) {
        self.interrupt.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Command::Stop);
    }
}

/// Spawn a VM thread for one script. Fails only when the thread cannot be created.
pub fn spawn_vm(options: StartVmOptions) -> std::io::Result<VmHandle> {
    let (tx, rx) = mpsc::channel();
    let interrupt = Arc::new(AtomicBool::new(false));
    let thread_interrupt = interrupt.clone();
    std::thread::Builder::new().name("pi-codemode-vm".to_string()).stack_size(VM_THREAD_STACK_SIZE).spawn(move || {
        let StartVmOptions { data, limits, post, on_exit } = options;
        struct ExitGuard(Option<Box<dyn FnOnce() + Send>>);
        impl Drop for ExitGuard {
            fn drop(&mut self) {
                if let Some(on_exit) = self.0.take() {
                    on_exit();
                }
            }
        }
        let _guard = ExitGuard(on_exit);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vm_main(data, limits, post.clone(), rx, thread_interrupt)));
        if let Err(panic) = result {
            let message = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string());
            post(VmReport::Message(WorkerToHostMessage::Crash { message: format!("VM panicked: {message}") }));
        }
    })?;
    Ok(VmHandle { tx, interrupt })
}

fn vm_main(
    data: VmStartData,
    limits: VmLimits,
    post: Arc<dyn Fn(VmReport) + Send + Sync>,
    rx: mpsc::Receiver<Command>,
    interrupt: Arc<AtomicBool>,
) {
    let mut vm = match Vm::create(&data, limits, post.clone(), interrupt.clone()) {
        Ok(vm) => vm,
        Err(message) => {
            post(VmReport::Message(WorkerToHostMessage::Crash { message }));
            return;
        }
    };
    vm.start(&data);
    while let Ok(command) = rx.recv() {
        if interrupt.load(Ordering::SeqCst) {
            break;
        }
        match command {
            Command::Deliver(message) => vm.deliver(message),
            Command::Stop => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Engine setup
// ---------------------------------------------------------------------------

/// Runs promise jobs one at a time, so the VM can poll its interrupt flag between them. There are
/// no timers: timeout and interval jobs are dropped.
#[derive(Default)]
struct Executor {
    jobs: RefCell<VecDeque<Job>>,
}

impl Executor {
    fn pop(&self) -> Option<Job> {
        self.jobs.borrow_mut().pop_front()
    }
}

impl JobExecutor for Executor {
    fn enqueue_job(self: Rc<Self>, job: Job, _context: &mut Context) {
        match job {
            Job::PromiseJob(_) | Job::GenericJob(_) | Job::AsyncJob(_) => self.jobs.borrow_mut().push_back(job),
            _ => {}
        }
    }

    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
        while let Some(job) = self.pop() {
            run_job(job, context)?;
        }
        Ok(())
    }
}

fn run_job(job: Job, context: &mut Context) -> JsResult<()> {
    match job {
        Job::PromiseJob(job) => job.call(context).map(|_| ()),
        Job::GenericJob(job) => job.call(context).map(|_| ()),
        Job::AsyncJob(job) => {
            let cell = RefCell::new(context);
            let future = job.call(&cell);
            let mut future = std::pin::pin!(future);
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            loop {
                if let std::task::Poll::Ready(result) = future.as_mut().poll(&mut cx) {
                    return result.map(|_| ());
                }
                std::thread::yield_now();
            }
        }
        _ => Ok(()),
    }
}

/// The virtual clock of a deterministic VM.
struct VirtualClock {
    started_at: f64,
    now: Cell<f64>,
}

struct VmClock {
    virtual_clock: Option<Rc<VirtualClock>>,
    started: Instant,
}

impl Clock for VmClock {
    fn now(&self) -> JsInstant {
        let elapsed = self.started.elapsed();
        JsInstant::new(elapsed.as_secs(), elapsed.subsec_nanos())
    }

    fn system_time_millis(&self) -> i64 {
        match &self.virtual_clock {
            Some(clock) => clock.now.get().round() as i64,
            None => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0),
        }
    }
}

struct VmHooks {
    utc: bool,
    /// The host timezone the execution names; `None`: this thread's own.
    timezone: Option<jiff::tz::TimeZone>,
    max_buffer_size: u64,
}

impl HostHooks for VmHooks {
    fn local_timezone_offset_seconds(&self, unix_time_seconds: i64) -> i32 {
        if self.utc {
            return 0;
        }
        match (&self.timezone, jiff::Timestamp::from_second(unix_time_seconds)) {
            (Some(timezone), Ok(at)) => timezone.to_offset(at).seconds(),
            _ => local_offset_seconds(unix_time_seconds),
        }
    }

    fn max_buffer_size(&self, _context: &mut Context) -> u64 {
        self.max_buffer_size
    }
}

/// A `TZ` value as a timezone, as the C library reads it: an IANA name (an optional leading `:`)
/// or a POSIX rule; anything else, including the empty string, is UTC.
pub fn timezone_named(tz: &str) -> jiff::tz::TimeZone {
    let name = tz.strip_prefix(':').unwrap_or(tz);
    if name.is_empty() {
        return jiff::tz::TimeZone::UTC;
    }
    jiff::tz::TimeZone::get(name)
        .or_else(|_| jiff::tz::TimeZone::posix(name))
        .unwrap_or(jiff::tz::TimeZone::UTC)
}

/// The host's UTC offset at `unix_time_seconds`, honoring `TZ`.
#[cfg(unix)]
fn local_offset_seconds(unix_time_seconds: i64) -> i32 {
    let time = unix_time_seconds as libc::time_t;
    // SAFETY: `localtime_r` writes into the provided `tm` and reads `time`; both outlive the call.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i32
    }
}

#[cfg(not(unix))]
fn local_offset_seconds(_unix_time_seconds: i64) -> i32 {
    0
}

/// QuickJS's `Math.random()`: xorshift64*, its state the first realtime clock reading in
/// microseconds, which upstream's deterministic VM makes `seed + 1` (so seed 0 does not become
/// seed 1's zero-state fix-up). QuickJS draws once while the context is created, so the script's
/// first value is the second draw. Same values as upstream for every seed.
struct Prng {
    state: Cell<u64>,
}

impl Prng {
    fn new(seed: u32) -> Self {
        let prng = Prng { state: Cell::new(u64::from(seed) + 1) };
        prng.next_f64();
        prng
    }

    fn next(&self) -> u64 {
        let mut x = self.state.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state.set(x);
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A double in [0, 1): 52 random mantissa bits in [1, 2), minus one.
    fn next_f64(&self) -> f64 {
        f64::from_bits((0x3ff << 52) | (self.next() >> 12)) - 1.0
    }
}

/// State the native functions reach through the context.
struct BridgeState {
    post: Arc<dyn Fn(VmReport) + Send + Sync>,
    interrupt: Arc<AtomicBool>,
    /// Calls the script has issued, tools and globals; reported with every `idle`.
    issued: Cell<u64>,
    /// The result being processed, 0 during the script's first run; reported with `done`.
    current: Cell<u64>,
    deterministic: bool,
    lossless_json: bool,
    /// A terminal message was reported; the next bridge call ends the script.
    settled: Cell<bool>,
    /// Loop iterations and promise jobs so far, and the budget for them.
    ticks: Cell<u64>,
    budget: Option<u64>,
    /// Why a tick stopped the script, kept because the engine discards the error it raised.
    stopped: Cell<Option<Stop>>,
    virtual_clock: Option<Rc<VirtualClock>>,
    started: Instant,
    prng: Option<Prng>,
}

impl BridgeState {
    fn send(&self, message: WorkerToHostMessage) {
        if message.is_terminal() {
            if self.settled.get() {
                return;
            }
            self.settled.set(true);
        }
        (self.post)(VmReport::Message(message));
    }

    fn halted(&self) -> bool {
        self.settled.get() || self.interrupt.load(Ordering::SeqCst)
    }

    /// Count one loop iteration or promise job. `Err` when the script must stop.
    fn tick(&self) -> Result<(), Stop> {
        if let Some(stop) = self.stopped.get() {
            return Err(stop);
        }
        let stop = if self.halted() {
            Some(Stop::Halted)
        } else {
            let ticks = self.ticks.get() + 1;
            self.ticks.set(ticks);
            self.budget.is_some_and(|budget| ticks > budget).then_some(Stop::Exhausted)
        };
        match stop {
            Some(stop) => {
                self.stopped.set(Some(stop));
                Err(stop)
            }
            None => Ok(()),
        }
    }
}

/// Why a tick stopped the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Exhausted,
    Halted,
}

fn state(context: &Context) -> &BridgeState {
    context.get_data::<BridgeState>().expect("codemode VM state")
}

fn string_arg(args: &[JsValue], index: usize) -> Option<String> {
    args.get(index).and_then(|value| value.as_string()).map(|s| s.to_std_string_lossy())
}

/// The error a native function returns to end the script: the script cannot catch it.
fn halt_error() -> JsError {
    RuntimeLimitError::LoopIteration.into()
}

/// `bridge(kind, a, b, c)`, called from the prelude with primitives only.
fn bridge(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let kind = string_arg(args, 0).unwrap_or_default();
    let st = state(context);
    if st.halted() {
        return Err(halt_error());
    }
    match kind.as_str() {
        "call" | "global" => {
            st.issued.set(st.issued.get() + 1);
            let id = args.get(1).and_then(JsValue::as_number).unwrap_or(0.0) as u64;
            let target = if kind == "call" { CodemodeCallTarget::Tool } else { CodemodeCallTarget::Global };
            if st.lossless_json {
                let name = args.get(2).and_then(JsValue::as_string).map(|s| s.to_vec()).unwrap_or_default();
                st.send(WorkerToHostMessage::CallUtf16 { id, target, name, args: string_arg(args, 3) });
            } else {
                st.send(WorkerToHostMessage::Call { id, target, name: string_arg(args, 2).unwrap_or_default(), args: string_arg(args, 3) });
            }
        }
        "output" => {
            let what = string_arg(args, 1).unwrap_or_default();
            if st.lossless_json {
                let units = |index: usize| args.get(index).and_then(JsValue::as_string).map(|s| s.to_vec()).unwrap_or_default();
                st.send(WorkerToHostMessage::OutputUtf16 { kind: what, data: units(2), mime_type: units(3) });
                return Ok(JsValue::undefined());
            }
            let data = string_arg(args, 2).unwrap_or_default();
            let item = match what.as_str() {
                "image" => CodemodeOutputItem::Image { data, mime_type: string_arg(args, 3).unwrap_or_default() },
                "console" => CodemodeOutputItem::Text { text: data, console: Some(true) },
                _ => CodemodeOutputItem::Text { text: data, console: None },
            };
            st.send(WorkerToHostMessage::Output { item });
        }
        "done" => {
            // The host stops the VM on `done`, so the `idle` of this drain never arrives: report its
            // numbers here.
            let during = st.deterministic.then(|| CodemodeDuring { seq: st.current.get(), issued: st.issued.get() });
            let ok = args.get(1).is_some_and(JsValue::to_boolean);
            let mut done = if ok {
                DoneMessage::success(string_arg(args, 2), string_arg(args, 3).unwrap_or_default())
            } else {
                DoneMessage::failure(string_arg(args, 2).unwrap_or_default())
            };
            done.during = during;
            st.send(WorkerToHostMessage::Done(done));
        }
        _ => {}
    }
    Ok(JsValue::undefined())
}

/// `__codemode$tick()`, called on every iteration of the script's loops.
fn tick(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    match state(context).tick() {
        Ok(()) => Ok(JsValue::undefined()),
        Err(_) => Err(halt_error()),
    }
}

fn performance_now(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let st = state(context);
    let now = match &st.virtual_clock {
        Some(clock) => clock.now.get() - clock.started_at,
        None => st.started.elapsed().as_secs_f64() * 1000.0,
    };
    Ok(JsValue::from(now))
}

fn math_random(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let st = state(context);
    let value = st.prng.as_ref().map(Prng::next_f64).unwrap_or(0.0);
    Ok(JsValue::from(value))
}

fn function(context: &mut Context, name: &str, length: usize, f: boa_engine::native_function::NativeFunctionPointer) -> JsObject {
    let realm = context.realm().clone();
    FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f)).name(JsString::from(name)).length(length).build().into()
}

fn js_string_value(text: &str) -> JsValue {
    JsValue::from(JsString::from(text))
}

/// Why the VM cannot continue.
enum Fatal {
    /// The interrupt budget or the loop iteration limit, with the message for the host.
    Exhausted(String),
    /// The recursion or VM stack limit, which QuickJS reports as a `RangeError`.
    StackOverflow,
    /// Interrupted by the host, or halted after a terminal message.
    Halted,
    /// Any other failure outside the script's control.
    Crash(String),
}

/// One VM. Field order matters: the GC handles drop before the context.
struct Vm {
    settle: JsObject,
    run: JsObject,
    stalled: JsObject,
    abandon: JsObject,
    api: JsObject,
    executor: Rc<Executor>,
    virtual_clock: Option<Rc<VirtualClock>>,
    next_seq: u64,
    early: BTreeMap<u64, HostToWorkerMessage>,
    dead: bool,
    fingerprint: Option<String>,
    limits: VmLimits,
    context: Context,
}

impl Vm {
    fn create(
        data: &VmStartData,
        limits: VmLimits,
        post: Arc<dyn Fn(VmReport) + Send + Sync>,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Vm, String> {
        let deterministic = data.deterministic;
        let virtual_clock = deterministic.map(|d| Rc::new(VirtualClock { started_at: d.started_at, now: Cell::new(d.started_at) }));
        let started = Instant::now();
        let executor = Rc::new(Executor::default());
        let mut context = Context::builder()
            .job_executor(executor.clone())
            .host_hooks(Rc::new(VmHooks {
                utc: deterministic.is_some(),
                timezone: data.timezone.as_deref().map(timezone_named),
                max_buffer_size: data.memory_limit_bytes.unwrap_or(1_610_612_736),
            }))
            .clock(Rc::new(VmClock { virtual_clock: virtual_clock.clone(), started }))
            .build()
            .map_err(|error| format!("Failed to create the VM: {error}"))?;
        {
            let runtime_limits = context.runtime_limits_mut();
            runtime_limits.set_recursion_limit(limits.recursion_limit);
            runtime_limits.set_backtrace_limit(32);
            // The prelude's own loops do not count; the limit applies from the script on.
            runtime_limits.disable_loop_iteration_limit();
        }
        context.insert_data(BridgeState {
            post,
            interrupt,
            issued: Cell::new(0),
            current: Cell::new(0),
            deterministic: deterministic.is_some(),
            lossless_json: data.lossless_json,
            settled: Cell::new(false),
            ticks: Cell::new(0),
            budget: limits.interrupt_budget,
            stopped: Cell::new(None),
            virtual_clock: virtual_clock.clone(),
            started,
            prng: deterministic.map(|d| Prng::new(d.seed)),
        });

        let setup = |context: &mut Context| -> JsResult<JsObject> {
            let global = context.global_object();
            // `performance.now()`, as QuickJS provides it.
            let performance = JsObject::with_object_proto(context.intrinsics());
            let now = function(context, "now", 0, performance_now);
            performance.set(js_string!("now"), now, true, context)?;
            global.set(js_string!("performance"), performance, true, context)?;
            let tick = function(context, "tick", 0, tick);
            global.define_property_or_throw(
                JsString::from(super::instrument::TICK),
                boa_engine::property::PropertyDescriptor::builder().value(tick).writable(false).enumerable(false).configurable(false),
                context,
            )?;
            if deterministic.is_some() {
                for name in ["WeakRef", "FinalizationRegistry", "Temporal"] {
                    global.delete_property_or_throw(PropertyKey::from(JsString::from(name)), context)?;
                }
                let math = global.get(js_string!("Math"), context)?;
                if let Some(math) = math.as_object() {
                    let random = function(context, "random", 0, math_random);
                    math.set(js_string!("random"), random, true, context)?;
                }
            }
            let prelude = context.eval(Source::from_bytes(PRELUDE_SOURCE.as_bytes()).with_path(Path::new("codemode-prelude.js")))?;
            let prelude =
                prelude.as_callable().ok_or_else(|| boa_engine::JsNativeError::typ().with_message("prelude is not a function"))?;
            let bridge = function(context, "bridge", 4, bridge);
            let tools = serde_json::to_string(&data.tools).unwrap_or_else(|_| "[]".into());
            let globals = serde_json::to_string(&data.globals).unwrap_or_else(|_| "[]".into());
            let store = data.store_json.clone().unwrap_or_else(|| serde_json::to_string(&data.store).unwrap_or_else(|_| "{}".into()));
            let api = prelude.call(
                &JsValue::undefined(),
                &[bridge.into(), js_string_value(&tools), js_string_value(&globals), js_string_value(&store)],
                context,
            )?;
            api.as_object().ok_or_else(|| boa_engine::JsNativeError::typ().with_message("prelude returned no API").into())
        };
        let api = setup(&mut context).map_err(|error| format!("Failed to start the VM: {}", describe_error(error, &mut context)))?;
        let member = |name: &str, context: &mut Context| -> Result<JsObject, String> {
            api.get(JsString::from(name), context)
                .ok()
                .and_then(|value| value.as_object())
                .ok_or_else(|| format!("Failed to start the VM: the prelude has no {name}"))
        };
        if let Some(limit) = limits.loop_iteration_limit {
            context.runtime_limits_mut().set_loop_iteration_limit(limit);
        }
        let settle = member("settle", &mut context)?;
        let run = member("run", &mut context)?;
        let stalled = member("stalled", &mut context)?;
        let abandon = member("abandon", &mut context)?;
        let fingerprint = deterministic.map(|_| {
            format!(
                "{ENGINE}, prelude {}, deterministic {DETERMINISTIC_VERSION}, stack {}, heap {}, budget {}{}",
                *PRELUDE_HASH,
                limits.recursion_limit,
                data.memory_limit_bytes.map_or_else(|| "none".to_string(), |bytes| bytes.to_string()),
                limits.interrupt_budget.map_or_else(|| "none".to_string(), |budget| budget.to_string()),
                if data.lossless_json { ", UTF16 JSON 3" } else { "" },
            )
        });
        Ok(Vm {
            settle,
            run,
            stalled,
            abandon,
            api,
            executor,
            virtual_clock,
            next_seq: 1,
            early: BTreeMap::new(),
            dead: false,
            fingerprint,
            limits,
            context,
        })
    }

    fn state(&self) -> &BridgeState {
        state(&self.context)
    }

    fn send(&self, message: WorkerToHostMessage) {
        self.state().send(message);
    }

    /// Evaluate the script and run it until it waits.
    fn start(&mut self, data: &VmStartData) {
        if let Some(fingerprint) = self.fingerprint.clone() {
            self.send(WorkerToHostMessage::Started { fingerprint });
        }
        // The prefix shares the first line with the script so reported line numbers match the
        // script as written.
        let wrapper = if let Some(code) = &data.code_utf16 {
            let mut wrapper: Vec<u16> = "(async (tools, console) => {".encode_utf16().collect();
            wrapper.extend_from_slice(code);
            wrapper.extend("\n})".encode_utf16());
            super::instrument::instrument_loops_utf16(&wrapper).unwrap_or(wrapper)
        } else {
            let wrapper = format!("(async (tools, console) => {{{}\n}})", data.code);
            super::instrument::instrument_loops(&wrapper).unwrap_or(wrapper).encode_utf16().collect()
        };
        let function = match self.context.eval(Source::from_utf16(&wrapper).with_path(Path::new("codemode.js"))) {
            Ok(function) => function,
            Err(error) => {
                if error.as_engine().is_some() {
                    let fatal = self.classify(&error);
                    self.fail(fatal);
                    return;
                }
                let text = describe_exception(error, &mut self.context);
                self.send(WorkerToHostMessage::Done(DoneMessage::failure(text)));
                return;
            }
        };
        let run = self.run.clone();
        let api: JsValue = self.api.clone().into();
        if let Err(error) = run.call(&api, &[function], &mut self.context) {
            let fatal = self.classify(&error);
            self.fail(fatal);
            return;
        }
        if self.drain().is_ok() && self.virtual_clock.is_some() {
            let issued = self.state().issued.get();
            self.send(WorkerToHostMessage::Idle { seq: 0, issued });
        }
    }

    /// Run queued jobs, then fail a script that waits on nothing that can ever resume it.
    fn drain(&mut self) -> Result<(), ()> {
        let mut jobs: u64 = 0;
        while let Some(job) = self.executor.pop() {
            if let Err(error) = run_job(job, &mut self.context) {
                // A job only fails for errors the script cannot catch.
                let fatal = self.classify(&error);
                self.fail(fatal);
                return Err(());
            }
            jobs += 1;
            let st = self.state();
            let stop = if st.budget.is_some() || jobs.is_multiple_of(JOBS_PER_POLL) { st.tick().err() } else { st.stopped.get() };
            if let Some(stop) = stop {
                let fatal = self.stop_fatal(stop);
                self.fail(fatal);
                return Err(());
            }
        }
        self.context.clear_kept_objects();
        // The engine discards errors raised inside an async function that resumed after an
        // await, so a tick that stopped the script is only visible here.
        if let Some(stop) = self.state().stopped.get() {
            let fatal = self.stop_fatal(stop);
            self.fail(fatal);
            return Err(());
        }
        let stalled = self.stalled.clone();
        let api: JsValue = self.api.clone().into();
        if let Err(error) = stalled.call(&api, &[], &mut self.context) {
            let fatal = self.classify(&error);
            self.fail(fatal);
            return Err(());
        }
        Ok(())
    }

    /// The script cannot continue: report why, unless the result is already decided.
    fn fail(&mut self, fatal: Fatal) {
        self.dead = true;
        let abandon = self.abandon.clone();
        let api: JsValue = self.api.clone().into();
        let halted = self.state().halted();
        // Clear the queue and stop the prelude from reporting anything else.
        self.executor.jobs.borrow_mut().clear();
        let _ = abandon.call(&api, &[], &mut self.context);
        if halted {
            return;
        }
        match fatal {
            Fatal::Halted => {}
            Fatal::Exhausted(message) => {
                self.state().settled.set(true);
                (self.state().post)(VmReport::Exhausted(message));
            }
            Fatal::StackOverflow => {
                let error = serde_json::json!({
                    "name": "RangeError",
                    "message": "Maximum call stack size exceeded",
                    "stack": "RangeError: Maximum call stack size exceeded",
                });
                let mut done = DoneMessage::failure(error.to_string());
                let st = self.state();
                if st.deterministic {
                    done.during = Some(CodemodeDuring { seq: st.current.get(), issued: st.issued.get() });
                }
                self.send(WorkerToHostMessage::Done(done));
            }
            Fatal::Crash(message) => self.send(WorkerToHostMessage::Crash { message }),
        }
    }

    fn apply(&mut self, message: &HostToWorkerMessage) -> Result<(), ()> {
        let (id, ok, payload, properties) = match message {
            HostToWorkerMessage::Result { id, ok, payload, properties, .. } => {
                (*id, *ok, payload.as_deref().map_or_else(JsValue::undefined, js_string_value), properties)
            }
            HostToWorkerMessage::ResultUtf16 { id, ok, payload, properties, .. } => {
                (*id, *ok, payload.as_ref().map_or_else(JsValue::undefined, |v| JsValue::from(JsString::from(v.as_slice()))), properties)
            }
        };
        let settle = self.settle.clone();
        let api: JsValue = self.api.clone().into();
        let args =
            [JsValue::from(id as f64), JsValue::from(ok), payload, properties.as_deref().map_or_else(JsValue::undefined, js_string_value)];
        if let Err(error) = settle.call(&api, &args, &mut self.context) {
            let fatal = self.classify(&error);
            self.fail(fatal);
            return Err(());
        }
        self.drain()
    }

    fn deliver(&mut self, message: HostToWorkerMessage) {
        if self.dead {
            return;
        }
        let Some(clock) = self.virtual_clock.clone() else {
            let _ = self.apply(&message);
            return;
        };
        let (id, seq) = match &message {
            HostToWorkerMessage::Result { id, seq, .. } => (*id, *seq),
            HostToWorkerMessage::ResultUtf16 { id, seq, .. } => (*id, Some(*seq)),
        };
        let seq = match seq {
            Some(seq) if seq >= self.next_seq && !self.early.contains_key(&seq) => seq,
            _ => {
                let expected = self.next_seq;
                let seq = seq.map_or_else(|| "undefined".to_string(), |seq| seq.to_string());
                self.send(WorkerToHostMessage::Crash {
                    message: format!("Error: Result {id} has sequence number {seq}, expected {expected} or later, once"),
                });
                self.dead = true;
                return;
            }
        };
        self.early.insert(seq, message);
        while let Some(next) = self.early.remove(&self.next_seq) {
            let now = match &next {
                HostToWorkerMessage::Result { now, .. } => *now,
                HostToWorkerMessage::ResultUtf16 { now, .. } => Some(*now),
            };
            // Never backwards: a resumed host's clock may be behind the one that started the run.
            if let Some(now) = now
                && now.is_finite()
            {
                clock.now.set(clock.now.get().max(now));
            }
            self.state().current.set(self.next_seq);
            if self.apply(&next).is_err() {
                return;
            }
            let issued = self.state().issued.get();
            self.send(WorkerToHostMessage::Idle { seq: self.next_seq, issued });
            self.next_seq += 1;
        }
    }
}

impl Vm {
    fn stop_fatal(&self, stop: Stop) -> Fatal {
        match stop {
            Stop::Halted => Fatal::Halted,
            Stop::Exhausted => {
                Fatal::Exhausted(format!("Execution exceeded its interrupt budget of {}", self.limits.interrupt_budget.unwrap_or_default()))
            }
        }
    }

    fn classify(&self, error: &JsError) -> Fatal {
        if let Some(stop) = self.state().stopped.get() {
            return self.stop_fatal(stop);
        }
        if self.state().halted() {
            return Fatal::Halted;
        }
        match error.as_engine() {
            Some(EngineError::RuntimeLimit(RuntimeLimitError::LoopIteration)) => Fatal::Exhausted(format!(
                "Execution exceeded its loop iteration limit of {}",
                self.limits.loop_iteration_limit.unwrap_or_default()
            )),
            Some(EngineError::RuntimeLimit(_)) => Fatal::StackOverflow,
            Some(other) => Fatal::Crash(other.to_string()),
            None => Fatal::Crash(format!("Promise job failed: {error}")),
        }
    }
}

fn get_string(object: &JsObject, key: &str, context: &mut Context) -> Option<String> {
    let value = object.get(JsString::from(key), context).ok()?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    value.to_string(context).ok().map(|s| s.to_std_string_lossy())
}

/// A short description of an error outside the script (setup failures).
fn describe_error(error: JsError, context: &mut Context) -> String {
    match error.into_opaque(context) {
        Ok(value) => match value.as_object() {
            Some(object) => {
                let name = get_string(&object, "name", context).unwrap_or_else(|| "Error".into());
                let message = get_string(&object, "message", context).unwrap_or_default();
                format!("{name}: {message}")
            }
            None => value.display().to_string(),
        },
        Err(error) => error.to_string(),
    }
}

/// `{ name, message, stack }` of an exception thrown while compiling the script, as JSON.
fn describe_exception(error: JsError, context: &mut Context) -> String {
    let (name, message, stack) = match error.into_opaque(context) {
        Ok(value) => match value.as_object() {
            Some(object) => (
                get_string(&object, "name", context).unwrap_or_else(|| "Error".into()),
                get_string(&object, "message", context).unwrap_or_default(),
                get_string(&object, "stack", context),
            ),
            None => ("Error".to_string(), value.display().to_string(), None),
        },
        Err(error) => ("Error".to_string(), error.to_string(), None),
    };
    let head = if message.is_empty() { name.clone() } else { format!("{name}: {message}") };
    let mut frames = stack.map(|stack| stack.trim_end().to_string()).filter(|stack| !stack.is_empty());
    if frames.is_none()
        && let Some((line, column)) = parse_position(&message)
    {
        frames = Some(format!("    at codemode.js:{line}:{column}"));
    }
    let stack = match frames {
        Some(frames) => format!("{head}\n{frames}"),
        None => head,
    };
    serde_json::json!({ "name": name, "message": message, "stack": stack }).to_string()
}

/// `line L, col C` in a parser message.
fn parse_position(message: &str) -> Option<(u32, u32)> {
    let start = message.rfind("line ")?;
    let rest = &message[start + 5..];
    let (line, rest) = rest.split_once(", col")?;
    let column: String = rest.trim_start_matches([' ', ':']).chars().take_while(char::is_ascii_digit).collect();
    Some((line.trim().parse().ok()?, column.parse().ok()?))
}
