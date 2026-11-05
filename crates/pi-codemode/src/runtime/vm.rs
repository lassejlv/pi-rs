//! Native djs execution behind the transport-independent codemode VM protocol.
//! Persistent JavaScript values are GC handles; host callbacks suspend the interpreter.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::prelude_source::{PRELUDE_HASH, PRELUDE_SOURCE};
use super::protocol::{DoneMessage, HostToWorkerMessage, VmStartData, WorkerToHostMessage};
use crate::types::{CodemodeCallTarget, CodemodeDuring, CodemodeOutputItem};
use djs_core::compiler::CompileOptions;
use djs_core::heap::Handle;
use djs_core::interp::Status;
use djs_core::value::Value;
use djs_core::vm::{Vm as Engine, VmOptions, WeakRefs};

const DETERMINISTIC_VERSION: u32 = 6;
const FUEL_PER_POLL: u64 = 10_000;
const VM_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;
/// Default fuel limit for one synchronous invocation, including generated code.
pub const DEFAULT_LOOP_ITERATION_LIMIT: u64 = 100_000_000;
/// Default recursion allowance, translated to djs value-stack slots.
pub const DEFAULT_RECURSION_LIMIT: usize = 4096;

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
    /// Instrumented iterations plus 10,000-fuel slices the execution may run. Part of
    /// the fingerprint.
    pub interrupt_budget: Option<u64>,
    /// Interpreter fuel one invocation may consume, including jobs and generated code.
    pub loop_iteration_limit: Option<u64>,
    /// Approximate JS call depth, translated to value-stack slots.
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
    std::thread::Builder::new()
        .name("pi-codemode-vm".to_string())
        .stack_size(VM_THREAD_STACK_SIZE)
        .spawn(move || {
            let StartVmOptions {
                data,
                limits,
                post,
                on_exit,
            } = options;
            struct ExitGuard(Option<Box<dyn FnOnce() + Send>>);
            impl Drop for ExitGuard {
                fn drop(&mut self) {
                    if let Some(on_exit) = self.0.take() {
                        on_exit();
                    }
                }
            }
            let _guard = ExitGuard(on_exit);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                vm_main(data, limits, post.clone(), rx, thread_interrupt)
            }));
            if let Err(panic) = result {
                let message = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown panic".to_string());
                post(VmReport::Message(WorkerToHostMessage::Crash {
                    message: format!("VM panicked: {message}"),
                }));
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

/// A `TZ` value interpreted as an IANA name or POSIX rule, with UTC fallback.
pub fn timezone_named(tz: &str) -> jiff::tz::TimeZone {
    let name = tz.strip_prefix(':').unwrap_or(tz);
    jiff::tz::TimeZone::get(name)
        .or_else(|_| jiff::tz::TimeZone::posix(name))
        .unwrap_or(jiff::tz::TimeZone::UTC)
}

struct Vm {
    engine: Engine,
    api: Option<Handle>,
    settle: Option<Handle>,
    run: Option<Handle>,
    stalled: Option<Handle>,
    post: Arc<dyn Fn(VmReport) + Send + Sync>,
    interrupt: Arc<AtomicBool>,
    clock: Option<f64>,
    timezone: jiff::tz::TimeZone,
    next_seq: u64,
    early: BTreeMap<u64, HostToWorkerMessage>,
    issued: u64,
    current: u64,
    ticks: u64,
    fuel_since_tick: u64,
    dead: bool,
    settled: bool,
    polling: bool,
    lossless_json: bool,
    fingerprint: Option<String>,
    limits: VmLimits,
}

impl Vm {
    fn create(
        data: &VmStartData,
        limits: VmLimits,
        post: Arc<dyn Fn(VmReport) + Send + Sync>,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let opts = VmOptions {
            memory_limit: usize::try_from(data.memory_limit_bytes.unwrap_or(1_610_612_736))
                .unwrap_or(usize::MAX),
            weak_refs: if data.deterministic.is_some() {
                WeakRefs::Absent
            } else {
                WeakRefs::default()
            },
            ..VmOptions::default()
        };
        let engine =
            djs_prelude::new_vm(opts).map_err(|e| format!("Failed to create the VM: {e:?}"))?;
        let timezone = if data.deterministic.is_some() {
            jiff::tz::TimeZone::UTC
        } else {
            data.timezone
                .as_deref()
                .map(timezone_named)
                .unwrap_or_else(jiff::tz::TimeZone::system)
        };
        let mut vm = Self {
            engine, api: None, settle: None, run: None, stalled: None, post, interrupt,
            clock: data.deterministic.map(|d| d.started_at), timezone, next_seq: 1, early: BTreeMap::new(),
            issued: 0, current: 0, ticks: 0, fuel_since_tick: 0, dead: false, settled: false, polling: false, lossless_json: data.lossless_json,
            fingerprint: data.deterministic.map(|_| format!(
                "djs {:016x}, prelude {}, deterministic {DETERMINISTIC_VERSION}, stack {}, heap {}, budget {}, fuel per poll {FUEL_PER_POLL}, invocation fuel {}{}",
                djs_core::vm::BUILD_HASH, *PRELUDE_HASH, limits.recursion_limit,
                data.memory_limit_bytes.map_or_else(|| "none".into(), |n| n.to_string()),
                limits.interrupt_budget.map_or_else(|| "none".into(), |n| n.to_string()),
                limits.loop_iteration_limit.map_or_else(|| "none".into(), |n| n.to_string()),
                if data.lossless_json { ", UTF16 JSON 3" } else { "" })), limits,
        };
        let bridge = vm
            .engine
            .new_host_function(0, "bridge")
            .map_err(|e| format!("{e:?}"))?;
        let bridge = vm.root(bridge)?;
        let tick = vm
            .engine
            .new_host_function(1, "tick")
            .map_err(|e| format!("{e:?}"))?;
        vm.engine
            .define_global(super::instrument::TICK, tick)
            .map_err(|e| format!("{e:?}"))?;
        let origin = data.deterministic.map_or_else(wall_time, |d| d.started_at);
        let setup = vm.eval(&format!(r#"Object.defineProperty(globalThis, {tick_name}, {{value: globalThis[{tick_name}], writable:false, configurable:false}});
            Object.defineProperty(globalThis, "performance", {{value: {{now() {{return Date.now() - {origin};}}}}, configurable:true}});
            delete globalThis.Temporal;"#, tick_name=crate::js::quote(super::instrument::TICK)))?;
        vm.free(setup);
        let factory = vm.eval(&PRELUDE_SOURCE)?;
        let tools = vm.string(&serde_json::to_string(&data.tools).map_err(|e| e.to_string())?)?;
        let globals =
            vm.string(&serde_json::to_string(&data.globals).map_err(|e| e.to_string())?)?;
        let store = vm.string(&data.store_json.clone().unwrap_or_else(|| {
            serde_json::to_string(&data.store).unwrap_or_else(|_| "{}".into())
        }))?;
        let api = vm.invoke(factory, None, &[bridge, tools, globals, store])?;
        vm.api = Some(api);
        for h in [factory, bridge, tools, globals, store] {
            vm.free(h);
        }
        for name in ["settle", "run", "stalled"] {
            let getter = vm.eval(&format!("(o => o.{name})"))?;
            let member = vm.invoke(getter, None, &[api])?;
            vm.free(getter);
            match name {
                "settle" => vm.settle = Some(member),
                "run" => vm.run = Some(member),
                _ => vm.stalled = Some(member),
            }
        }
        // The adapter and prelude need stack space even when the script's limit is tiny.
        let mut opts = vm.engine.options();
        opts.exec.max_stack =
            u32::try_from(limits.recursion_limit.saturating_mul(32).max(2048)).unwrap_or(u32::MAX);
        vm.engine.set_options(opts);
        if let Some(d) = data.deterministic {
            let mut seed = u64::from(d.seed) + 1;
            seed ^= seed >> 12;
            seed ^= seed << 25;
            seed ^= seed >> 27;
            vm.engine.set_random_seed(seed);
        } else {
            vm.engine.set_random_seed(origin.to_bits());
        }
        vm.polling = true;
        Ok(vm)
    }

    fn root(&mut self, value: Value) -> Result<Handle, String> {
        self.engine
            .heap_mut()
            .new_handle(value)
            .map_err(|e| format!("{e:?}"))
    }
    fn free(&mut self, handle: Handle) {
        self.engine.heap_mut().drop_handle(handle);
    }
    fn value(&self, handle: Handle) -> Value {
        self.engine.heap().get_handle(handle)
    }
    fn string(&mut self, text: &str) -> Result<Handle, String> {
        let s = self
            .engine
            .heap_mut()
            .alloc_str(text)
            .map_err(|e| format!("{e:?}"))?;
        self.root(Value::string(s))
    }
    fn utf16(&mut self, text: &[u16]) -> Result<Handle, String> {
        let s = self
            .engine
            .heap_mut()
            .alloc_utf16(text)
            .map_err(|e| format!("{e:?}"))?;
        self.root(Value::string(s))
    }
    fn send(&mut self, message: WorkerToHostMessage) {
        if self.settled {
            return;
        }
        if message.is_terminal() {
            self.settled = true;
        }
        (self.post)(VmReport::Message(message));
    }
    fn fail(&mut self, message: String) {
        self.engine.abort();
        self.dead = true;
        if !self.settled && !self.interrupt.load(Ordering::SeqCst) {
            if message.starts_with("Execution exceeded") {
                self.settled = true;
                (self.post)(VmReport::Exhausted(message));
            } else {
                self.send(WorkerToHostMessage::Crash { message });
            }
        }
    }
    fn set_clock(&mut self) {
        let now = self.clock.unwrap_or_else(wall_time);
        // djs returns this value from `Date.now()` unchanged. ECMAScript time values are
        // integer milliseconds: round a deterministic virtual clock, truncate wall time.
        let now = if self.clock.is_some() {
            now.round()
        } else {
            now.trunc()
        };
        self.engine.set_now(now);
        let at =
            jiff::Timestamp::from_millisecond(now as i64).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
        self.engine
            .set_time_zone_offset(self.timezone.to_offset(at).seconds() / 60);
    }
    fn eval(&mut self, source: &str) -> Result<Handle, String> {
        let f = self
            .engine
            .compile(
                source.as_bytes(),
                &CompileOptions::eval("codemode-prelude.js"),
            )
            .map_err(|e| e.to_string())?;
        let f = self.root(f)?;
        let result = self.invoke(f, None, &[]);
        self.free(f);
        result
    }
    fn invoke(
        &mut self,
        function: Handle,
        this: Option<Handle>,
        args: &[Handle],
    ) -> Result<Handle, String> {
        let args: Vec<_> = args.iter().map(|&h| self.value(h)).collect();
        self.engine
            .call(
                self.value(function),
                this.map_or(Value::UNDEFINED, |h| self.value(h)),
                &args,
            )
            .map_err(|e| format!("{e:?}"))?;
        let mut fuel = 0u64;
        loop {
            if self.polling && (self.interrupt.load(Ordering::SeqCst) || self.settled) {
                self.engine.abort();
                return Err("Execution interrupted".into());
            }
            self.set_clock();
            let (status, used) = self.engine.run_metered(FUEL_PER_POLL);
            if self.polling {
                fuel = fuel.saturating_add(used);
                self.fuel_since_tick = self.fuel_since_tick.saturating_add(used);
                let ticks = self.fuel_since_tick / FUEL_PER_POLL;
                self.fuel_since_tick %= FUEL_PER_POLL;
                self.charge(ticks)?;
                if self
                    .limits
                    .loop_iteration_limit
                    .is_some_and(|limit| fuel > limit)
                {
                    return Err(format!(
                        "Execution exceeded its loop iteration limit of {}",
                        self.limits.loop_iteration_limit.unwrap_or_default()
                    ));
                }
            }
            match status {
                Status::OutOfFuel => {}
                Status::HostCall { call_id, fn_id } => {
                    let info = self.engine.host_call(call_id).ok_or("Missing host call")?;
                    if fn_id == 0 {
                        self.bridge(&info.args)?;
                    } else if fn_id == 1 {
                        if self.polling {
                            self.charge(1)?;
                        }
                    } else {
                        return Err(format!("Unknown host function {fn_id}"));
                    }
                    self.engine
                        .resolve_host_call(call_id, Ok(Value::UNDEFINED))
                        .map_err(|e| format!("{e:?}"))?;
                }
                Status::Done(value) => return self.root(value),
                Status::Idle => return self.root(Value::UNDEFINED),
                Status::Threw(value) => return Err(self.engine.display(value)),
                Status::HostOutOfMemory => return Err("out of host memory".into()),
            }
        }
    }
    fn charge(&mut self, ticks: u64) -> Result<(), String> {
        self.ticks = self.ticks.saturating_add(ticks);
        if self
            .limits
            .interrupt_budget
            .is_some_and(|budget| self.ticks > budget)
        {
            Err(format!(
                "Execution exceeded its interrupt budget of {}",
                self.limits.interrupt_budget.unwrap_or_default()
            ))
        } else {
            Ok(())
        }
    }
    fn text_arg(&self, args: &[Value], index: usize) -> Option<String> {
        args.get(index)
            .and_then(|v| v.as_string())
            .map(|s| self.engine.heap().str_to_string(s))
    }
    fn units_arg(&self, args: &[Value], index: usize) -> Vec<u16> {
        args.get(index)
            .and_then(|v| v.as_string())
            .map(|s| self.engine.heap().str_to_utf16(s))
            .unwrap_or_default()
    }
    fn bridge(&mut self, args: &[Value]) -> Result<(), String> {
        if self.settled || self.interrupt.load(Ordering::SeqCst) {
            return Err("Execution interrupted".into());
        }
        match self.text_arg(args, 0).as_deref() {
            Some("call" | "global") => {
                self.issued += 1;
                let id = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as u64;
                let target = if self.text_arg(args, 0).as_deref() == Some("call") {
                    CodemodeCallTarget::Tool
                } else {
                    CodemodeCallTarget::Global
                };
                let payload = self.text_arg(args, 3);
                if self.lossless_json {
                    self.send(WorkerToHostMessage::CallUtf16 {
                        id,
                        target,
                        name: self.units_arg(args, 2),
                        args: payload,
                    });
                } else {
                    self.send(WorkerToHostMessage::Call {
                        id,
                        target,
                        name: self.text_arg(args, 2).unwrap_or_default(),
                        args: payload,
                    });
                }
            }
            Some("output") => {
                let kind = self.text_arg(args, 1).unwrap_or_default();
                if self.lossless_json {
                    self.send(WorkerToHostMessage::OutputUtf16 {
                        kind,
                        data: self.units_arg(args, 2),
                        mime_type: self.units_arg(args, 3),
                    });
                } else {
                    let text = self.text_arg(args, 2).unwrap_or_default();
                    let item = match kind.as_str() {
                        "image" => CodemodeOutputItem::Image {
                            data: text,
                            mime_type: self.text_arg(args, 3).unwrap_or_default(),
                        },
                        "console" => CodemodeOutputItem::Text {
                            text,
                            console: Some(true),
                        },
                        _ => CodemodeOutputItem::Text {
                            text,
                            console: None,
                        },
                    };
                    self.send(WorkerToHostMessage::Output { item });
                }
            }
            Some("done") => {
                let mut done = if args.get(1).and_then(|v| v.as_bool()) == Some(true) {
                    DoneMessage::success(
                        self.text_arg(args, 2),
                        self.text_arg(args, 3).unwrap_or_default(),
                    )
                } else {
                    DoneMessage::failure(self.text_arg(args, 2).unwrap_or_default())
                };
                done.during = self.clock.map(|_| CodemodeDuring {
                    seq: self.current,
                    issued: self.issued,
                });
                self.send(WorkerToHostMessage::Done(done));
            }
            _ => {}
        }
        Ok(())
    }
    fn start(&mut self, data: &VmStartData) {
        if let Some(fingerprint) = self.fingerprint.clone() {
            self.send(WorkerToHostMessage::Started { fingerprint });
        }
        let wrapper = if let Some(code) = &data.code_utf16 {
            let mut wrapper: Vec<u16> = "(async (tools, console) => {".encode_utf16().collect();
            wrapper.extend_from_slice(code);
            wrapper.extend("\n})".encode_utf16());
            utf16_source(&super::instrument::instrument_loops_utf16(&wrapper).unwrap_or(wrapper))
        } else {
            let wrapper = format!("(async (tools, console) => {{{}\n}})", data.code);
            super::instrument::instrument_loops(&wrapper).unwrap_or(wrapper)
        };
        let function = match self
            .engine
            .compile(wrapper.as_bytes(), &CompileOptions::script("codemode.js"))
        {
            Ok(f) => f,
            Err(e) => {
                let error = serde_json::json!({"name":"SyntaxError", "message":e.message, "stack":format!("SyntaxError: {}\n    at codemode.js:{}:{}", e.message, e.line, e.col)});
                self.send(WorkerToHostMessage::Done(DoneMessage::failure(
                    error.to_string(),
                )));
                return;
            }
        };
        let result = (|| {
            let f = self.root(function)?;
            let function = self.invoke(f, None, &[])?;
            self.free(f);
            let result = self.invoke(self.run.ok_or("Missing run")?, self.api, &[function]);
            self.free(function);
            let value = result?;
            self.free(value);
            self.drain()
        })();
        if let Err(e) = result {
            self.fail(e);
        }
        if self.clock.is_some() && !self.dead {
            self.send(WorkerToHostMessage::Idle {
                seq: 0,
                issued: self.issued,
            });
        }
    }
    fn drain(&mut self) -> Result<(), String> {
        if self.settled {
            return Ok(());
        }
        let value = self.invoke(self.stalled.ok_or("Missing stalled")?, self.api, &[])?;
        self.free(value);
        Ok(())
    }
    fn apply(&mut self, message: &HostToWorkerMessage) -> Result<(), ()> {
        let result = (|| {
            let (id, ok, payload, properties) = match message {
                HostToWorkerMessage::Result {
                    id,
                    ok,
                    payload,
                    properties,
                    ..
                } => (
                    *id,
                    *ok,
                    match payload {
                        Some(p) => self.string(p)?,
                        None => self.root(Value::UNDEFINED)?,
                    },
                    properties,
                ),
                HostToWorkerMessage::ResultUtf16 {
                    id,
                    ok,
                    payload,
                    properties,
                    ..
                } => (
                    *id,
                    *ok,
                    match payload {
                        Some(p) => self.utf16(p)?,
                        None => self.root(Value::UNDEFINED)?,
                    },
                    properties,
                ),
            };
            let id = self.root(Value::number(id as f64))?;
            let ok = self.root(Value::bool(ok))?;
            let properties = match properties {
                Some(p) => self.string(p)?,
                None => self.root(Value::UNDEFINED)?,
            };
            let args = [id, ok, payload, properties];
            let result = self.invoke(self.settle.ok_or("Missing settle")?, self.api, &args);
            for h in args {
                self.free(h);
            }
            let value = result?;
            self.free(value);
            self.drain()
        })();
        result.map_err(|e| self.fail(e))
    }
    fn deliver(&mut self, message: HostToWorkerMessage) {
        if self.dead {
            return;
        }
        let Some(clock) = self.clock else {
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
                self.clock = Some(self.clock.unwrap_or(clock).max(now));
            }
            self.current = self.next_seq;
            if self.apply(&next).is_err() {
                return;
            }
            let issued = self.issued;
            self.send(WorkerToHostMessage::Idle {
                seq: self.next_seq,
                issued,
            });
            self.next_seq += 1;
        }
    }
}

fn wall_time() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

/// UTF-16 source as UTF-8 for the parser. Lone surrogates (only meaningful inside string and
/// template literals) become `\uXXXX` escapes, which denote the same code unit there.
fn utf16_source(units: &[u16]) -> String {
    let mut out = String::with_capacity(units.len());
    for unit in char::decode_utf16(units.iter().copied()) {
        match unit {
            Ok(c) => out.push(c),
            Err(e) => out.push_str(&format!("\\u{:04X}", e.unpaired_surrogate())),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prelude_handles_survive_collection_before_start_and_delivery() {
        let data: VmStartData = serde_json::from_value(serde_json::json!({
            "code": "return (await tools.echo({value: 'kept'})).value",
            "tools": [{"name": "echo", "jsName": "echo", "description": ""}],
            "globals": [], "store": {},
            "deterministic": {"startedAt": 1000, "seed": 1}
        }))
        .expect("valid VM configuration");
        let (tx, rx) = mpsc::channel();
        let mut vm = Vm::create(
            &data,
            VmLimits::default(),
            Arc::new(move |report| {
                tx.send(report).expect("report receiver is alive");
            }),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("create VM");
        vm.engine.collect().expect("collect before starting");
        vm.start(&data);
        let reports: Vec<_> = rx.try_iter().collect();
        assert!(
            reports.iter().any(|r| matches!(
                r,
                VmReport::Message(WorkerToHostMessage::Call { id: 1, .. })
            )),
            "{reports:?}"
        );
        vm.engine.collect().expect("collect suspended script");
        vm.deliver(HostToWorkerMessage::Result {
            id: 1,
            ok: true,
            payload: Some(r#"{"value":"kept"}"#.into()),
            seq: Some(1),
            now: Some(2000.0),
            properties: None,
        });
        let reports: Vec<_> = rx.try_iter().collect();
        assert!(reports.iter().any(|r| matches!(r, VmReport::Message(WorkerToHostMessage::Done(done)) if done.ok && done.value.as_deref() == Some(r#""kept""#))), "{reports:?}");
    }
}
