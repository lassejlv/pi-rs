//! Host side of an execution, shared by every place a VM can run.
//!
//! The host owns everything the script must not influence: the tool table, call records, output,
//! the deadline, the caller's abort signal, and the decoding of what the VM reports. The VM itself
//! runs somewhere a [`VmLauncher`] decides: a dedicated thread ([`CodemodeSandbox::new`]) or
//! another process or machine reached over RPC ([`CodemodeSandbox::remote`]). The two sides only
//! exchange the messages in [`super::protocol`], so the launchers differ in transport and
//! termination, not in behavior.
//!
//! Lifecycle of one execution:
//! 1. Execution start arms the deadline and abort listener, then asks the launcher for a channel.
//! 2. Messages from the VM arrive through [`VmEvents`]; tool calls run here and their results go
//!    back through [`VmChannel::post`]. Replies produced before the launcher has returned its
//!    channel are queued and flushed when it arrives.
//! 3. The first terminal event wins: `done`, `crash`, a launcher failure, the deadline, or an
//!    abort. Finishing aborts pending tool calls, asks the channel to stop, and resolves the result
//!    once that request completes. A VM thread or remote VM may exit later. Later events are
//!    ignored.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::protocol::{DoneMessage, HostToWorkerMessage, VmGlobal, VmStartData, VmTool, WorkerToHostMessage};
use crate::abort::AbortSignal;
use crate::identifier::{is_identifier, to_codemode_identifier};
use crate::js;
use crate::json_run::{self, *};
use crate::types::*;
use pi_json::strict::{self, JsonString, Value as JsonValue};

/// Default deadline of an execution in ms.
pub const DEFAULT_TIMEOUT_MS: f64 = 300_000.0;

const RESERVED_GLOBALS: [&str; 9] = ["tools", "ALL_TOOLS", "console", "text", "image", "exit", "globalThis", "store", "load"];

/// The host's handle on a running VM.
pub trait VmChannel: Send + Sync {
    fn post(&self, message: HostToWorkerMessage);
    /// Requests VM shutdown. Called once; may resolve before a VM thread or remote VM actually
    /// exits.
    fn stop(&self) -> BoxFuture<'static, ()>;
}

/// Starts a VM for one execution. Failures become `sandbox` errors with the failure's message. The
/// VM may start reporting messages before the returned future resolves.
pub trait VmLauncher: Send + Sync {
    fn launch(&self, data: VmStartData, events: VmEvents) -> BoxFuture<'static, Result<Box<dyn VmChannel>, String>>;
}

pub(crate) enum VmEvent {
    Message(WorkerToHostMessage),
    Raw(Value),
    Failure(CodemodeError),
}

/// Callbacks a launcher uses to report what its VM does. Cheap to clone; usable from any thread.
#[derive(Clone)]
pub struct VmEvents {
    tx: mpsc::UnboundedSender<VmEvent>,
}

impl VmEvents {
    pub(crate) fn from_sender(tx: mpsc::UnboundedSender<VmEvent>) -> Self {
        VmEvents { tx }
    }

    /// A message from the VM.
    pub fn message(&self, message: WorkerToHostMessage) {
        let _ = self.tx.send(VmEvent::Message(message));
    }

    /// A message received as JSON. Validated by the host, so launchers can pass through anything
    /// they receive; a malformed message fails the execution as a broken bridge.
    pub fn message_json(&self, message: Value) {
        let _ = self.tx.send(VmEvent::Raw(message));
    }

    /// The VM failed or stopped outside the bridge, for example a thread that could not start or a
    /// remote error.
    pub fn failure(&self, error: CodemodeError) {
        let _ = self.tx.send(VmEvent::Failure(error));
    }
}

/// Errors of the sandbox API itself (not of scripts, which come back as results).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CodemodeSandboxError(pub String);

/// Options every sandbox accepts, wherever its VM runs.
#[derive(Clone, Default)]
pub struct CodemodeSandboxBaseOptions {
    pub tools: Vec<CodemodeTool>,
    /// Functions exposed as top-level identifiers instead of on `tools`, for host helpers such as
    /// attaching an image to the result. They behave like tools (JSON round trip, promise result)
    /// but are not recorded in the result's calls. Names must be identifiers (or
    /// `namespace.member`) and may not shadow the built-in globals (`tools`, `ALL_TOOLS`,
    /// `console`, `text`, `image`, `exit`, `store`, `load`).
    pub globals: Vec<CodemodeTool>,
    /// Overall deadline per execution in ms, including time spent in tools. Non-finite values
    /// disable the deadline. Default: 300000.
    pub timeout_ms: Option<f64>,
    /// Maximum memory the VM may allocate (QuickJS's heap limit). It is part of the deterministic
    /// fingerprint.
    pub memory_limit_bytes: Option<u64>,
}

struct BridgeError(String);

fn parse_bridge_json(json: &str, what: &str) -> Result<Value, BridgeError> {
    serde_json::from_str(json).map_err(|_| BridgeError(format!("{what} is not valid JSON")))
}

fn parse_store_writes(json: &str) -> Result<CodemodeStoreWrites, BridgeError> {
    let entries = parse_bridge_json(json, "store writes")?;
    let Value::Array(entries) = entries else {
        return Err(BridgeError("store writes are not an array".into()));
    };
    let mut writes = CodemodeStoreWrites::default();
    for entry in entries {
        let malformed = || BridgeError("store writes contain a malformed entry".into());
        let Value::Array(entry) = entry else { return Err(malformed()) };
        let Some(Value::String(key)) = entry.first() else { return Err(malformed()) };
        match entry.len() {
            1 => writes.delete.push(key.clone()),
            2 => {
                let Value::String(value) = &entry[1] else { return Err(malformed()) };
                let value = parse_bridge_json(value, &format!("store value for {}", js::quote(key)))?;
                writes.set.insert(key.clone(), value);
            }
            _ => return Err(malformed()),
        }
    }
    Ok(writes)
}

fn parse_script_error(json: &str) -> Result<CodemodeError, BridgeError> {
    let parsed = parse_bridge_json(json, "script error")?;
    let malformed = || BridgeError("script error is malformed".into());
    let object = match parsed {
        Value::Object(object) => object,
        Value::Array(_) => return Err(malformed()),
        _ => return Err(BridgeError("script error is not an object".into())),
    };
    let optional = |key: &str| -> Result<Option<String>, BridgeError> {
        match object.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(malformed()),
        }
    };
    let Some(Value::String(message)) = object.get("message") else { return Err(malformed()) };
    Ok(CodemodeError { kind: CodemodeErrorKind::Script, name: optional("name")?, message: message.clone(), stack: optional("stack")? })
}

/// Checks global names: identifiers or `namespace.member`, not shadowing a built-in global or each
/// other.
pub(crate) fn check_globals<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<(), CodemodeSandboxError> {
    let mut seen = HashSet::new();
    let mut namespaces = Vec::new();
    for name in names {
        let parts: Vec<&str> = name.split('.').collect();
        if parts.len() > 2 || !parts.iter().all(|part| is_identifier(part)) || RESERVED_GLOBALS.contains(&parts[0]) {
            return Err(CodemodeSandboxError(format!("Invalid global name \"{name}\"")));
        }
        if !seen.insert(name.to_string()) {
            return Err(CodemodeSandboxError(format!("Global \"{name}\" is already registered")));
        }
        if parts.len() == 2 && !namespaces.contains(&parts[0]) {
            namespaces.push(parts[0]);
        }
    }
    for namespace in namespaces {
        if seen.contains(namespace) {
            return Err(CodemodeSandboxError(format!("Global \"{namespace}\" conflicts with the namespace \"{namespace}\"")));
        }
    }
    Ok(())
}

fn legacy_vm_tools(tools: &[CodemodeDeclaredTool]) -> Vec<VmTool> {
    tools
        .iter()
        .map(|tool| VmTool {
            name: tool.name.clone().into(),
            js_name: tool.js_name.clone().unwrap_or_else(|| to_codemode_identifier(&tool.name)).into(),
            description: tool.description.clone().unwrap_or_default().into(),
        })
        .collect()
}
fn json_vm_tools(tools: &[CodemodeJsonDeclaredTool]) -> Vec<VmTool> {
    tools
        .iter()
        .map(|tool| VmTool {
            name: tool.name.clone(),
            js_name: tool.js_name.clone().unwrap_or_else(|| crate::to_json_codemode_identifier(&tool.name).into()),
            description: tool.description.clone().unwrap_or_default(),
        })
        .collect()
}
fn serialize_store(store: Option<&Map<String, Value>>) -> IndexMap<String, String> {
    let mut serialized = IndexMap::new();
    if let Some(store) = store {
        for key in js::object_keys(store) {
            serialized.insert(key.clone(), serde_json::to_string(&store[key.as_str()]).unwrap_or_else(|_| "null".into()));
        }
    }
    serialized
}

/// `reason instanceof Error ? reason.message : "Execution aborted"`. A signal aborted without a
/// reason carries Node's default reason, a `DOMException` ("This operation was aborted").
fn abort_message(signal: &AbortSignal) -> String {
    signal.reason().unwrap_or_else(|| DEFAULT_ABORT_REASON.to_string())
}

/// The message of the `DOMException` Node uses as the reason of `controller.abort()`.
pub const DEFAULT_ABORT_REASON: &str = "This operation was aborted";

struct PendingCall {
    record: Option<usize>,
    started_at: Instant,
    token: CancellationToken,
}

/// Callbacks of a host-controlled run: its calls go to the caller, which answers with `deliver()`.
#[derive(Clone)]
enum CallHandler {
    Typed(OnCall),
    Json(OnJsonCall),
}

#[derive(Clone)]
struct Control {
    on_started: Option<OnStarted>,
    on_call: CallHandler,
    on_idle: Option<OnIdle>,
}

#[derive(Default)]
struct Executors {
    tools: HashMap<String, CodemodeTool>,
    globals: HashMap<String, CodemodeTool>,
}

struct ExecutionOptions {
    code: String,
    code_utf16: Option<Vec<u16>>,
    tools: Vec<VmTool>,
    globals: Vec<CodemodeDeclaredGlobal>,
    executors: Executors,
    timeout_ms: f64,
    signal: Option<AbortSignal>,
    memory_limit_bytes: Option<u64>,
    store: IndexMap<String, String>,
    store_json: Option<String>,
    deterministic: Option<super::protocol::VmDeterministic>,
    timezone: Option<String>,
    control: Option<Control>,
    launcher: Arc<dyn VmLauncher>,
}

#[derive(Default)]
struct ExecState {
    channel: Option<Arc<dyn VmChannel>>,
    unsent: Vec<HostToWorkerMessage>,
    delivered: HashSet<u64>,
    output: Vec<JsonValue>,
    calls: Vec<json_run::JsonCallRecord>,
    pending: HashMap<u64, PendingCall>,
    finished: bool,
}

/// One script run in its own VM.
pub(crate) struct Execution {
    state: Mutex<ExecState>,
    executors: Executors,
    control: Option<Control>,
    finished: AtomicBool,
    result_tx: Mutex<Option<oneshot::Sender<JsonValue>>>,
    /// Cancelled when the execution has its outcome; stops the host loop.
    ended: CancellationToken,
    /// Cancelled once the result has been resolved.
    completed: CancellationToken,
    runtime: tokio::runtime::Handle,
}

impl Execution {
    fn start(options: ExecutionOptions) -> (Arc<Execution>, oneshot::Receiver<JsonValue>) {
        let runtime = tokio::runtime::Handle::current();
        let (result_tx, result_rx) = oneshot::channel();
        let execution = Arc::new(Execution {
            state: Mutex::new(ExecState::default()),
            executors: options.executors,
            control: options.control,
            finished: AtomicBool::new(false),
            result_tx: Mutex::new(Some(result_tx)),
            ended: CancellationToken::new(),
            completed: CancellationToken::new(),
            runtime: runtime.clone(),
        });

        if let Some(signal) = &options.signal
            && signal.aborted()
        {
            execution.finish(Err(CodemodeError::new(CodemodeErrorKind::Aborted, abort_message(signal))), None);
            return (execution, result_rx);
        }

        let data = VmStartData {
            code: options.code,
            code_utf16: options.code_utf16,
            tools: options.tools,
            globals: options.globals.iter().map(|global| VmGlobal { name: global.name.clone(), spread: global.spread }).collect(),
            memory_limit_bytes: options.memory_limit_bytes,
            store: options.store,
            store_json: options.store_json,
            lossless_json: execution.control.as_ref().is_some_and(|c| matches!(c.on_call, CallHandler::Json(_))),
            deterministic: options.deterministic,
            timezone: options.timezone,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let events = VmEvents { tx };

        let timeout_ms = options.timeout_ms;
        let deadline = if timeout_ms.is_finite() {
            Some(Duration::try_from_secs_f64(timeout_ms.max(0.0) / 1000.0).unwrap_or(Duration::MAX))
        } else {
            None
        };
        runtime.spawn(host_loop(execution.clone(), rx, deadline, timeout_ms, options.signal));

        let launch = options.launcher.launch(data, events);
        let launched = execution.clone();
        runtime.spawn(async move {
            match launch.await {
                Ok(channel) => launched.set_channel(Arc::from(channel)),
                Err(message) => launched.finish(Err(CodemodeError::new(CodemodeErrorKind::Sandbox, message)), None),
            }
        });
        (execution, result_rx)
    }

    fn set_channel(&self, channel: Arc<dyn VmChannel>) {
        let mut state = self.state.lock();
        if state.finished {
            drop(state);
            self.runtime.spawn(channel.stop());
            return;
        }
        for message in std::mem::take(&mut state.unsent) {
            channel.post(message);
        }
        state.channel = Some(channel);
    }

    fn post(state: &mut ExecState, message: HostToWorkerMessage) {
        match &state.channel {
            Some(channel) => channel.post(message),
            None => state.unsent.push(message),
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    /// Ends the execution with `kind: "aborted"` (unless it already ended) and waits for its result
    /// to resolve.
    pub(crate) async fn abort(&self, message: &str) {
        self.finish(Err(CodemodeError::new(CodemodeErrorKind::Aborted, message)), None);
        self.completed.cancelled().await;
    }

    /// An exception here would leave the execution unsettled.
    fn receive(self: &Arc<Self>, event: VmEvent) {
        let message = match event {
            VmEvent::Failure(error) => {
                self.finish(Err(error), None);
                return;
            }
            VmEvent::Message(message) => message,
            VmEvent::Raw(value) => match super::protocol::parse_worker_to_host_message(value) {
                Some(message) => message,
                None => {
                    self.bridge_broken(BridgeError("unknown message from the worker".into()));
                    return;
                }
            },
        };
        if let Err(error) = self.handle_message(message) {
            self.bridge_broken(error);
        }
    }

    fn bridge_broken(&self, error: BridgeError) {
        self.finish(
            Err(CodemodeError::new(
                CodemodeErrorKind::Sandbox,
                format!("Sandbox bridge broken: {}. The script may have modified built-ins such as a prototype's toJSON.", error.0),
            )),
            None,
        );
    }

    fn handle_message(self: &Arc<Self>, message: WorkerToHostMessage) -> Result<(), BridgeError> {
        if self.is_finished() {
            return Ok(());
        }
        match message {
            WorkerToHostMessage::Output { item } => {
                let mut state = self.state.lock();
                if !state.finished {
                    state.output.push(json_run::encoded(&item));
                }
            }
            WorkerToHostMessage::OutputUtf16 { kind, data, mime_type } => {
                if !self.control.as_ref().is_some_and(|c| matches!(c.on_call, CallHandler::Json(_))) {
                    return Err(BridgeError("unknown message from the worker".into()));
                }
                let mut item = JsonValue::Object(Default::default());
                let image = kind == "image";
                json_run::put(&mut item, "type", if image { "image" } else { "text" });
                json_run::put(&mut item, if image { "data" } else { "text" }, JsonValue::String(JsonString::from_utf16(data)));
                if image {
                    json_run::put(&mut item, "mimeType", JsonValue::String(JsonString::from_utf16(mime_type)));
                }
                if kind == "console" {
                    json_run::put(&mut item, "console", JsonValue::Bool(true));
                }
                let mut state = self.state.lock();
                if !state.finished {
                    state.output.push(item);
                }
            }
            WorkerToHostMessage::Call { id, target, name, args } => {
                if self.state.lock().pending.contains_key(&id) {
                    return Err(BridgeError(format!("duplicate call id {id}")));
                }
                match self.control.clone() {
                    Some(control) => self.forward_call(id, target, name.into(), args, &control)?,
                    None => self.handle_call(id, target, name, args),
                }
            }
            WorkerToHostMessage::CallUtf16 { id, target, name, args } => {
                if self.state.lock().pending.contains_key(&id) {
                    return Err(BridgeError(format!("duplicate call id {id}")));
                }
                let Some(control) = self.control.clone().filter(|c| matches!(c.on_call, CallHandler::Json(_))) else {
                    return Err(BridgeError("unexpected UTF16 call in an ordinary run".into()));
                };
                self.forward_call(id, target, JsonString::from_utf16(name), args, &control)?;
            }
            WorkerToHostMessage::Started { fingerprint } => {
                if self.control.as_ref().is_some_and(|c| matches!(c.on_call, CallHandler::Json(_)))
                    && !fingerprint.ends_with(", UTF16 JSON 3")
                {
                    return Err(BridgeError("the sandbox does not support lossless JSON runs".into()));
                }
                if let Some(on_started) = self.control.as_ref().and_then(|control| control.on_started.clone()) {
                    on_started(&fingerprint);
                }
            }
            WorkerToHostMessage::Idle { seq, issued } => {
                if let Some(on_idle) = self.control.as_ref().and_then(|control| control.on_idle.clone()) {
                    on_idle(seq, issued);
                }
            }
            WorkerToHostMessage::Done(done) => self.handle_done(done)?,
            WorkerToHostMessage::Crash { message } => self.finish(Err(CodemodeError::new(CodemodeErrorKind::Sandbox, message)), None),
        }
        Ok(())
    }

    fn handle_done(&self, done: DoneMessage) -> Result<(), BridgeError> {
        if self.control.as_ref().is_some_and(|c| matches!(c.on_call, CallHandler::Json(_))) {
            let result = json_run::done_json(&done).map_err(BridgeError)?;
            self.finish_json(result, done.during);
            return Ok(());
        }
        // Decode everything before finish(), which must not fail once it starts.
        if !done.ok {
            let error = parse_script_error(done.error.as_deref().unwrap_or("undefined"))?;
            self.finish(Err(error), done.during);
            return Ok(());
        }
        let value = match &done.value {
            Some(value) => Some(parse_bridge_json(value, "return value")?),
            None => None,
        };
        let store_writes = parse_store_writes(done.writes.as_deref().unwrap_or("undefined"))?;
        self.finish(Ok(CodemodeSuccess { value, store_writes }), done.during);
        Ok(())
    }

    /// A host-controlled run: record the call and hand it to the caller, which answers with
    /// `deliver()`.
    fn forward_call(
        self: &Arc<Self>,
        id: u64,
        target: CodemodeCallTarget,
        name: JsonString,
        args: Option<String>,
        control: &Control,
    ) -> Result<(), BridgeError> {
        enum Prepared {
            Typed(OnCall, CodemodeHostCall),
            Json(OnJsonCall, CodemodeJsonHostCall),
        }
        let prepared = match &control.on_call {
            CallHandler::Typed(callback) => Prepared::Typed(
                callback.clone(),
                CodemodeHostCall {
                    id,
                    target,
                    name: name.to_string_checked().map_err(|_| BridgeError("call name cannot be represented as UTF8".into()))?,
                    args: args.as_ref().map(|v| parse_bridge_json(v, "call arguments")).transpose()?,
                },
            ),
            CallHandler::Json(callback) => Prepared::Json(
                callback.clone(),
                CodemodeJsonHostCall {
                    id,
                    target,
                    name: name.clone(),
                    args: args
                        .as_ref()
                        .map(|v| strict::parse(v).map_err(|_| BridgeError("call arguments is not valid JSON".into())))
                        .transpose()?,
                },
            ),
        };
        {
            let mut state = self.state.lock();
            let record = (target == CodemodeCallTarget::Tool).then(|| {
                state.calls.push(json_run::JsonCallRecord { name: name.clone(), status: CodemodeCallStatus::Cancelled, duration_ms: 0.0 });
                state.calls.len() - 1
            });
            state.pending.insert(id, PendingCall { record, started_at: Instant::now(), token: CancellationToken::new() });
        }
        match prepared {
            Prepared::Typed(callback, call) => callback(call, CodemodeRunHandle::new(self.clone())),
            Prepared::Json(callback, call) => callback(call, CodemodeJsonRunHandle::new(self.clone())),
        };
        Ok(())
    }

    fn handle_call(self: &Arc<Self>, id: u64, target: CodemodeCallTarget, name: String, args: Option<String>) {
        let is_tool = target == CodemodeCallTarget::Tool;
        let token = CancellationToken::new();
        {
            let mut state = self.state.lock();
            let record = is_tool.then(|| {
                state.calls.push(json_run::JsonCallRecord {
                    name: name.clone().into(),
                    status: CodemodeCallStatus::Cancelled,
                    duration_ms: 0.0,
                });
                state.calls.len() - 1
            });
            state.pending.insert(id, PendingCall { record, started_at: Instant::now(), token: token.clone() });
        }
        let tool = if is_tool { self.executors.tools.get(&name) } else { self.executors.globals.get(&name) };
        let future: BoxFuture<'static, CodemodeToolResult> = match tool {
            None => {
                let message = format!("Unknown {} \"{name}\"", if is_tool { "tool" } else { "global" });
                Box::pin(async move { Err(message) })
            }
            Some(tool) => {
                let parsed = match &args {
                    Some(args) => serde_json::from_str::<Value>(args).map(Some).map_err(|error| error.to_string()),
                    None => Ok(None),
                };
                match parsed {
                    Ok(args) => (tool.execute)(args, CodemodeToolContext { signal: token }),
                    Err(message) => Box::pin(async move { Err(message) }),
                }
            }
        };
        let mut future = std::panic::AssertUnwindSafe(future).catch_unwind();
        // Poll once right away, so a tool's synchronous work happens in message order: an
        // unawaited global still runs before the script settles.
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        if let std::task::Poll::Ready(result) = std::pin::Pin::new(&mut future).poll(&mut cx) {
            self.complete_call(id, panic_to_error(result));
            return;
        }
        let execution = self.clone();
        self.runtime.spawn(async move {
            let result = future.await;
            execution.complete_call(id, panic_to_error(result));
        });
    }

    fn complete_call(&self, id: u64, result: CodemodeToolResult) {
        let (status, reply) = match result {
            Ok(value) => (
                CodemodeCallStatus::Ok,
                HostToWorkerMessage::Result {
                    id,
                    ok: true,
                    payload: value.map(|value| serde_json::to_string(&value).unwrap_or_else(|_| "null".into())),
                    seq: None,
                    now: None,
                    properties: None,
                },
            ),
            Err(message) => (
                CodemodeCallStatus::Error,
                HostToWorkerMessage::Result { id, ok: false, payload: Some(message), seq: None, now: None, properties: None },
            ),
        };
        let mut state = self.state.lock();
        // Already cancelled by finish(): the record keeps "cancelled" and the VM is gone or going.
        let Some(pending) = state.pending.remove(&id) else { return };
        if let Some(index) = pending.record {
            let record = &mut state.calls[index];
            record.status = status;
            record.duration_ms = pending.started_at.elapsed().as_secs_f64() * 1000.0;
        }
        Self::post(&mut state, reply);
    }

    fn finish(&self, outcome: Result<CodemodeSuccess, CodemodeError>, during: Option<CodemodeDuring>) {
        let result = json_run::encoded(&CodemodeResult { outcome, output: vec![], calls: vec![], during });
        self.finish_json(result, during);
    }
    fn finish_json(&self, mut result: JsonValue, during: Option<CodemodeDuring>) {
        let mut state = self.state.lock();
        if state.finished {
            return;
        }
        state.finished = true;
        self.finished.store(true, Ordering::SeqCst);
        for pending in state.pending.values() {
            pending.token.cancel();
        }
        let pending: Vec<PendingCall> = state.pending.drain().map(|(_, pending)| pending).collect();
        for pending in pending {
            if let Some(index) = pending.record {
                state.calls[index].duration_ms = pending.started_at.elapsed().as_secs_f64() * 1000.0;
            }
        }
        state.unsent.clear();
        json_run::put(&mut result, "output", JsonValue::Array(std::mem::take(&mut state.output).into()));
        json_run::put(&mut result, "calls", json_run::encoded(&std::mem::take(&mut state.calls)));
        if let Some(during) = during {
            json_run::put(&mut result, "during", json_run::encoded(&during));
        }
        let channel = state.channel.clone();
        drop(state);
        self.ended.cancel();
        let result_tx = self.result_tx.lock().take();
        let completed = self.completed.clone();
        let resolve = move || {
            if let Some(tx) = result_tx {
                let _ = tx.send(result);
            }
            completed.cancel();
        };
        match channel {
            None => resolve(),
            Some(channel) => {
                let stop = channel.stop();
                self.runtime.spawn(async move {
                    stop.await;
                    resolve();
                });
            }
        }
    }
}

fn panic_to_error(result: std::thread::Result<CodemodeToolResult>) -> CodemodeToolResult {
    result.unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown panic".to_string());
        Err(format!("Tool panicked: {message}"))
    })
}

impl RunControl for Execution {
    fn finished(&self) -> bool {
        self.is_finished()
    }

    fn deliver(&self, seq: u64, now: f64, id: u64, outcome: CodemodeOutcome) -> Result<(), String> {
        let mut state = self.state.lock();
        if state.finished {
            return Ok(());
        }
        if !now.is_finite() {
            return Err(format!("The clock value of result {seq} is not a finite number"));
        }
        if seq < 1 || state.delivered.contains(&seq) {
            return Err(format!("Result number {seq} is not a positive integer used once"));
        }
        if !state.pending.contains_key(&id) {
            return Err(format!("Call {id} is not waiting for a result"));
        }
        let (ok, payload, properties) = match outcome {
            CodemodeOutcome::Ok { value } => {
                (true, value.map(|value| serde_json::to_string(&value).unwrap_or_else(|_| "null".into())), None)
            }
            CodemodeOutcome::Err { message, properties } => {
                let properties = match properties {
                    None => None,
                    Some(Value::Object(properties)) => Some(Value::Object(properties).to_string()),
                    Some(_) => return Err(format!("The properties of result {seq} are not an object")),
                };
                (false, Some(message), properties)
            }
        };
        state.delivered.insert(seq);
        let pending = state.pending.remove(&id).expect("pending call");
        if let Some(index) = pending.record {
            let record = &mut state.calls[index];
            record.status = if ok { CodemodeCallStatus::Ok } else { CodemodeCallStatus::Error };
            record.duration_ms = pending.started_at.elapsed().as_secs_f64() * 1000.0;
        }
        Self::post(&mut state, HostToWorkerMessage::Result { id, ok, payload, seq: Some(seq), now: Some(now), properties });
        Ok(())
    }
    fn deliver_json(&self, seq: u64, now: f64, id: u64, outcome: CodemodeJsonOutcome) -> Result<(), String> {
        let mut state = self.state.lock();
        if state.finished {
            return Ok(());
        }
        if !now.is_finite() {
            return Err(format!("The clock value of result {seq} is not a finite number"));
        }
        if seq < 1 || state.delivered.contains(&seq) {
            return Err(format!("Result number {seq} is not a positive integer used once"));
        }
        if !state.pending.contains_key(&id) {
            return Err(format!("Call {id} is not waiting for a result"));
        }
        let (ok, payload, properties) = match outcome {
            CodemodeJsonOutcome::Ok { value } => (true, value.map(|v| v.to_string().encode_utf16().collect()), None),
            CodemodeJsonOutcome::Err { message, properties } => {
                (false, Some(message.units().to_vec()), properties.map(|v| JsonValue::Object(v.into()).to_string()))
            }
        };
        state.delivered.insert(seq);
        let pending = state.pending.remove(&id).expect("pending call");
        if let Some(index) = pending.record {
            state.calls[index].status = if ok { CodemodeCallStatus::Ok } else { CodemodeCallStatus::Error };
            state.calls[index].duration_ms = pending.started_at.elapsed().as_secs_f64() * 1000.0;
        }
        Self::post(&mut state, HostToWorkerMessage::ResultUtf16 { id, ok, payload, seq, now, properties });
        Ok(())
    }
}

async fn host_loop(
    execution: Arc<Execution>,
    mut rx: mpsc::UnboundedReceiver<VmEvent>,
    deadline: Option<Duration>,
    timeout_ms: f64,
    signal: Option<AbortSignal>,
) {
    let timer = async {
        match deadline {
            Some(deadline) => tokio::time::sleep(deadline).await,
            None => std::future::pending().await,
        }
    };
    let aborted = async {
        match &signal {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(timer, aborted);
    let mut open = true;
    loop {
        tokio::select! {
            biased;
            _ = execution.ended.cancelled() => break,
            event = rx.recv(), if open => match event {
                Some(event) => execution.receive(event),
                None => open = false,
            },
            _ = &mut aborted => {
                let message = signal.as_ref().map(abort_message).unwrap_or_default();
                execution.finish(Err(CodemodeError::new(CodemodeErrorKind::Aborted, message)), None);
            }
            _ = &mut timer => {
                execution.finish(
                    Err(CodemodeError::new(
                        CodemodeErrorKind::Timeout,
                        format!("Execution timed out after {} ms", js::number_to_string(timeout_ms)),
                    )),
                    None,
                );
            }
        }
    }
}

struct SandboxInner {
    tools: RwLock<IndexMap<String, CodemodeTool>>,
    globals: IndexMap<String, CodemodeTool>,
    timeout_ms: f64,
    memory_limit_bytes: Option<u64>,
    launcher: Arc<dyn VmLauncher>,
    running: Mutex<HashMap<u64, Arc<Execution>>>,
    next_execution: AtomicU64,
    closed: AtomicBool,
}

/// Runs model-written JavaScript where the only capability is calling injected tools.
///
/// The script sees `tools.<name>(args)` for every registered tool, `ALL_TOOLS`, the output helpers
/// `text`, `image`, `exit`, and `console.*`, `store`/`load`, `performance.now()`, and the
/// configured globals; nothing else (no timers, `fetch`, `process`, `require`, modules). Each
/// `execute()` gets its own VM; the sandbox only holds the tool table and defaults. `close()`
/// aborts in-flight executions. Cheap to clone; clones share the tool table.
#[derive(Clone)]
pub struct CodemodeSandbox {
    inner: Arc<SandboxInner>,
}

impl std::fmt::Debug for CodemodeSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodemodeSandbox").field("tools", &self.inner.tools.read().len()).finish_non_exhaustive()
    }
}

/// The TS name of the shared sandbox class.
pub type CodemodeSandboxBase = CodemodeSandbox;

/// Removes a finished or abandoned execution from the sandbox; aborts it when the `execute()`
/// future is dropped before the result.
struct RunningGuard {
    sandbox: Arc<SandboxInner>,
    id: u64,
    execution: Arc<Execution>,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.sandbox.running.lock().remove(&self.id);
        if !self.execution.is_finished() {
            self.execution.finish(Err(CodemodeError::new(CodemodeErrorKind::Aborted, "Execution aborted")), None);
        }
    }
}

impl CodemodeSandbox {
    /// A sandbox whose VMs run where `launcher` puts them.
    pub fn with_launcher(
        options: CodemodeSandboxBaseOptions,
        launcher: Arc<dyn VmLauncher>,
    ) -> Result<CodemodeSandbox, CodemodeSandboxError> {
        let mut tools = IndexMap::new();
        for tool in options.tools {
            if tools.contains_key(&tool.name) {
                return Err(CodemodeSandboxError(format!("Tool \"{}\" is already registered", tool.name)));
            }
            tools.insert(tool.name.clone(), tool);
        }
        check_globals(options.globals.iter().map(|global| global.name.as_str()))?;
        let globals = options.globals.into_iter().map(|global| (global.name.clone(), global)).collect();
        Ok(CodemodeSandbox {
            inner: Arc::new(SandboxInner {
                tools: RwLock::new(tools),
                globals,
                timeout_ms: options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
                memory_limit_bytes: options.memory_limit_bytes,
                launcher,
                running: Mutex::new(HashMap::new()),
                next_execution: AtomicU64::new(1),
                closed: AtomicBool::new(false),
            }),
        })
    }

    /// Fails if a tool with the same name is already registered.
    pub fn register_tool(&self, tool: CodemodeTool) -> Result<(), CodemodeSandboxError> {
        let mut tools = self.inner.tools.write();
        if tools.contains_key(&tool.name) {
            return Err(CodemodeSandboxError(format!("Tool \"{}\" is already registered", tool.name)));
        }
        tools.insert(tool.name.clone(), tool);
        Ok(())
    }

    pub fn unregister_tool(&self, name: &str) -> bool {
        self.inner.tools.write().shift_remove(name).is_some()
    }

    pub fn tools(&self) -> Vec<CodemodeTool> {
        self.inner.tools.read().values().cloned().collect()
    }

    pub fn globals(&self) -> Vec<CodemodeTool> {
        self.inner.globals.values().cloned().collect()
    }

    fn register(&self, execution: Arc<Execution>) -> RunningGuard {
        let id = self.inner.next_execution.fetch_add(1, Ordering::Relaxed);
        self.inner.running.lock().insert(id, execution.clone());
        RunningGuard { sandbox: self.inner.clone(), id, execution }
    }

    /// Run `code` as an async function body: `return` and top-level `await` work. Fails only when
    /// the sandbox is closed; script failures come back as results with `ok() == false`. The script
    /// can use `store(key, value)` and `load(key)` on `options.store`. Dropping the future aborts
    /// the execution.
    pub async fn execute(&self, code: &str, options: CodemodeExecuteOptions) -> Result<CodemodeResult, CodemodeSandboxError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(CodemodeSandboxError("Sandbox is closed".into()));
        }
        let tools: Vec<CodemodeTool> = self.inner.tools.read().values().cloned().collect();
        let executors = Executors {
            tools: tools.iter().map(|tool| (tool.name.clone(), tool.clone())).collect(),
            globals: self.inner.globals.iter().map(|(name, tool)| (name.clone(), tool.clone())).collect(),
        };
        let (execution, result) = Execution::start(ExecutionOptions {
            code: code.to_string(),
            code_utf16: None,
            tools: tools
                .iter()
                .map(|tool| VmTool {
                    name: tool.name.clone().into(),
                    js_name: to_codemode_identifier(&tool.name).into(),
                    description: tool.description.clone().unwrap_or_default().into(),
                })
                .collect(),
            globals: self
                .inner
                .globals
                .values()
                .map(|global| CodemodeDeclaredGlobal { name: global.name.clone(), spread: global.spread })
                .collect(),
            executors,
            timeout_ms: options.timeout_ms.unwrap_or(self.inner.timeout_ms),
            signal: options.signal,
            memory_limit_bytes: self.inner.memory_limit_bytes,
            store: serialize_store(options.store.as_ref()),
            store_json: None,
            deterministic: None,
            timezone: options.timezone,
            control: None,
            launcher: self.inner.launcher.clone(),
        });
        let _guard = self.register(execution);
        let result = result.await.map_err(|_| CodemodeSandboxError("The execution ended without a result".into()))?;
        json_run::decode_result(result).map_err(CodemodeSandboxError)
    }

    /// Run `code` deterministically, with the caller answering its calls: the same script, start
    /// time, seed, tools, globals, and store, answered with the same results in the same `seq`
    /// order, makes the same calls with the same arguments and produces the same output, on any VM
    /// with the same fingerprint. A host can therefore replay it after a restart. The VM runs in
    /// UTC, its clock reads `started_at` and then the `now` of the result being processed, its
    /// randomness comes from `seed`, and `WeakRef` and `FinalizationRegistry` do not exist.
    ///
    /// The sandbox's own tools and globals are not used: the run declares its own, and every call
    /// goes to `options.on_call`. Must be called within a tokio runtime.
    pub fn start(&self, code: &str, options: CodemodeRunOptions) -> Result<CodemodeRun, CodemodeSandboxError> {
        let tools = legacy_vm_tools(&options.tools);
        self.check_start(options.started_at, &tools, &options.globals)?;
        let (execution, result) = Execution::start(ExecutionOptions {
            code: code.to_string(),
            code_utf16: None,
            tools,
            globals: options.globals,
            executors: Executors::default(),
            timeout_ms: options.timeout_ms.unwrap_or(f64::INFINITY),
            signal: options.signal,
            memory_limit_bytes: self.inner.memory_limit_bytes,
            store: serialize_store(options.store.as_ref()),
            store_json: None,
            deterministic: Some(super::protocol::VmDeterministic { started_at: options.started_at, seed: options.seed }),
            timezone: None,
            control: Some(Control {
                on_started: options.on_started,
                on_call: CallHandler::Typed(options.on_call),
                on_idle: options.on_idle,
            }),
            launcher: self.inner.launcher.clone(),
        });
        let guard = self.register(execution.clone());
        let result = async move {
            let result = result.await;
            drop(guard);
            result.ok().and_then(|value| json_run::decode_result(value).ok()).unwrap_or_else(|| CodemodeResult {
                outcome: Err(CodemodeError::new(CodemodeErrorKind::Sandbox, "The execution ended without a result")),
                output: Vec::new(),
                calls: Vec::new(),
                during: None,
            })
        };
        Ok(CodemodeRun { result: Box::pin(result), control: execution })
    }

    fn check_start(&self, started_at: f64, tools: &[VmTool], globals: &[CodemodeDeclaredGlobal]) -> Result<(), CodemodeSandboxError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(CodemodeSandboxError("Sandbox is closed".into()));
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(CodemodeSandboxError("start() must be called within a tokio runtime".into()));
        }
        if !started_at.is_finite() || started_at < 0.0 {
            return Err(CodemodeSandboxError("startedAt must be a non-negative number of milliseconds since the epoch".into()));
        }
        let names: HashSet<&JsonString> = tools.iter().map(|tool| &tool.name).collect();
        if names.len() != tools.len() {
            return Err(CodemodeSandboxError("Tool names must be unique".into()));
        }
        let script_names: HashSet<&JsonString> = tools.iter().map(|tool| &tool.js_name).collect();
        if script_names.len() != tools.len() {
            return Err(CodemodeSandboxError("Tool script names must be unique".into()));
        }
        check_globals(globals.iter().map(|global| global.name.as_str()))?;
        Ok(())
    }
    /// Deterministic execution with lossless JavaScript JSON values.
    pub fn start_json(&self, code: &str, options: CodemodeJsonRunOptions) -> Result<CodemodeJsonRun, CodemodeSandboxError> {
        self.start_json_utf16(&code.encode_utf16().collect::<Vec<_>>(), options)
    }

    /// Deterministic JSON execution preserving every source-text code unit.
    pub fn start_json_utf16(&self, code: &[u16], options: CodemodeJsonRunOptions) -> Result<CodemodeJsonRun, CodemodeSandboxError> {
        let tools = json_vm_tools(&options.tools);
        self.check_start(options.started_at, &tools, &options.globals)?;
        let (code, code_utf16) = match String::from_utf16(code) {
            Ok(code) => (code, None),
            Err(_) => (String::new(), Some(code.to_vec())),
        };
        let store_json = options.store.as_ref().map(|store| {
            JsonValue::Object(
                store.iter().map(|(key, value)| (key.clone(), JsonValue::String(value.to_string().into()))).collect::<strict::Map>().into(),
            )
            .to_string()
        });
        let (execution, result) = Execution::start(ExecutionOptions {
            code,
            code_utf16,
            tools,
            globals: options.globals,
            executors: Executors::default(),
            timeout_ms: options.timeout_ms.unwrap_or(f64::INFINITY),
            signal: options.signal,
            memory_limit_bytes: self.inner.memory_limit_bytes,
            store: IndexMap::new(),
            store_json,
            deterministic: Some(super::protocol::VmDeterministic { started_at: options.started_at, seed: options.seed }),
            timezone: None,
            control: Some(Control {
                on_started: options.on_started,
                on_call: CallHandler::Json(options.on_call),
                on_idle: options.on_idle,
            }),
            launcher: self.inner.launcher.clone(),
        });
        let guard = self.register(execution.clone());
        let result = async move {
            let value = result.await;
            drop(guard);
            value.unwrap_or_else(|_| {
                json_run::encoded(&CodemodeResult {
                    outcome: Err(CodemodeError::new(CodemodeErrorKind::Sandbox, "The execution ended without a result")),
                    output: vec![],
                    calls: vec![],
                    during: None,
                })
            })
        };
        Ok(CodemodeJsonRun { result: Box::pin(result), handle: CodemodeJsonRunHandle::new(execution) })
    }

    /// Aborts in-flight executions (they resolve with `kind: "aborted"`) and rejects new ones.
    pub async fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        let running: Vec<Arc<Execution>> = self.inner.running.lock().values().cloned().collect();
        futures::future::join_all(running.iter().map(|execution| execution.abort("Sandbox closed"))).await;
    }

    /// Whether `close()` was called.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }
}
