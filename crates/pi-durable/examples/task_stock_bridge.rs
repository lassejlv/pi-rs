//! Callback transport for unmodified upstream task tests. This process owns the
//! Session, scheduler and invocation state. JS supplies the original task code
//! and original test backend; no scheduling decisions are made in the adapter.
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_chord::context::{ContextKey, create_context_key, with_context_value};
use pi_chord::{
    AbortController,
    context::{AbortError, AbortReason as SignalReason, with_abort_signal},
};
use pi_durable::{
    harness::{runtime::TaskRuntime as NativeRuntime, scheduler::*, state::NextTaskState},
    storage::{Adopt, StorageFuture},
    *,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, Write},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
#[path = "support/bound_conversation_bridge.rs"]
mod bound_conversation_bridge;
#[path = "support/builtin_bridge.rs"]
mod builtin_bridge;
#[path = "support/codemode_bridge.rs"]
mod codemode_bridge;
#[path = "support/codemode_vm_bridge.rs"]
mod codemode_vm_bridge;
#[path = "support/compaction_bridge.rs"]
mod compaction_bridge;
#[path = "support/context_bridge.rs"]
mod context_bridge;
#[path = "support/document_bridge.rs"]
mod document_bridge;
#[path = "support/generation_bridge.rs"]
mod generation_bridge;
#[path = "support/task_graph_bridge.rs"]
mod graph_bridge;
#[path = "support/memory_bridge.rs"]
mod memory_bridge;
#[path = "support/model_bridge.rs"]
mod model_bridge;
#[path = "support/prompt_bridge.rs"]
mod prompt_bridge;
#[path = "support/registry_bridge.rs"]
mod registry_bridge;
#[path = "support/task_remote_storage.rs"]
mod remote_storage;
#[path = "support/session_bridge.rs"]
mod session_bridge;
#[path = "support/strict_bridge.rs"]
mod strict_bridge;
#[path = "support/submission_bridge.rs"]
mod submission_bridge;
#[path = "support/telemetry_bridge.rs"]
mod telemetry_bridge;
#[path = "support/tool_bridge.rs"]
mod tool_bridge;
#[path = "support/tool_environment.rs"]
mod tool_environment;
#[path = "support/view_bridge.rs"]
mod view_bridge;
static CALL_CONTEXT: LazyLock<ContextKey<HostContext>> =
    LazyLock::new(|| create_context_key("stock-test context"));
struct HostContext {
    id: u64,
    hub: std::sync::Weak<Hub>,
}
impl Drop for HostContext {
    fn drop(&mut self) {
        if let Some(hub) = self.hub.upgrade() {
            hub.emit(object([
                ("type", encode(&"context.drop")),
                ("id", encode(&self.id)),
            ]));
        }
    }
}
static HARNESS_CONTEXT: LazyLock<ContextKey<u64>> =
    LazyLock::new(|| create_context_key("stock-test harness"));
fn native_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64
}
fn encode(value: &impl Serialize) -> JsonValue {
    pi_chord::json::to_value(value).unwrap()
}
fn object<const N: usize>(fields: [(&str, JsonValue); N]) -> JsonValue {
    JsonValue::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect::<pi_chord::json::Map>()
            .into(),
    )
}
fn decode<T: DeserializeOwned>(value: &JsonValue) -> Result<T> {
    serde_json::from_str(&value.to_string()).map_err(Error::from)
}
fn field<T: DeserializeOwned>(value: &JsonValue, key: &str) -> Result<T> {
    decode(&value[key])
}
fn task_ownership(options: &JsonValue) -> Result<TaskOwnership> {
    match options["ownership"]["kind"].to_utf8().as_deref() {
        Some("task") => Ok(TaskOwnership::Task(field(&options["ownership"], "taskId")?)),
        Some("conversation") => Ok(TaskOwnership::Conversation),
        _ => Err(Error::message("Task ownership is required")),
    }
}
static HOST_ERRORS: std::sync::LazyLock<Mutex<Vec<(Error, u64)>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));
fn error_value(error: &Error) -> JsonValue {
    let mut value = object([
        ("name", encode(&error.name())),
        ("message", encode(&error.message_text())),
        (
            "aborted",
            encode(&matches!(error.kind(), ErrorKind::Aborted(_))),
        ),
    ]);
    if let ErrorKind::SessionFailed { cause }
    | ErrorKind::DocumentCopy { cause, .. }
    | ErrorKind::Caused { cause, .. } = error.kind()
    {
        value
            .as_object_mut()
            .unwrap()
            .insert("cause".into(), error_value(cause));
    }
    if let Some((_, id)) = HOST_ERRORS
        .lock()
        .iter()
        .find(|(held, _)| held.same_error(error))
    {
        value
            .as_object_mut()
            .unwrap()
            .insert("foreignId".into(), encode(id));
    }
    if let ErrorKind::ReadAfterWrite(method) = error.kind() {
        value.as_object_mut().unwrap().extend([
            ("kind".into(), encode(&"readAfterWrite")),
            ("method".into(), encode(method)),
        ]);
    }
    if let ErrorKind::ConversationBusy(conversation) = error.kind() {
        value.as_object_mut().unwrap().extend([
            ("kind".into(), encode(&"conversationBusy")),
            ("conversationId".into(), encode(conversation)),
        ]);
    }
    if let ErrorKind::ReadOnly(operation) = error.kind() {
        value.as_object_mut().unwrap().extend([
            ("kind".into(), encode(&"readOnly")),
            ("operation".into(), encode(operation)),
        ]);
    }
    value
}
type TaskRuntime = NativeRuntime<JsonValue, JsonValue, JsonValue, u64>;
type JsonTask = Task<JsonValue, JsonValue, JsonValue, u64>;
type ErasedRuntime = NativeRuntime<JsonValue, JsonValue, JsonValue, ()>;
enum Pending {
    Async(tokio::sync::oneshot::Sender<Result<Option<JsonValue>>>),
    Sync(std::sync::mpsc::Sender<Result<Option<JsonValue>>>),
}
/// Where the Hub writes its frames (one strict-JSON text per frame): stdout for the stock
/// process, an in-process queue for the embedded runtime (`pi-durable-jsrt`).
pub(crate) type Output = Box<dyn FnMut(&str) + Send>;
pub(crate) struct Hub {
    output: Mutex<Output>,
    next: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    host_pending: Mutex<HashSet<u64>>,
    phase_serials: Mutex<HashMap<u64, u64>>,
    abort_receipts: Mutex<HashMap<(u64, TaskId), HashSet<u64>>>,
    changed: tokio::sync::Notify,
    input_closed: AtomicBool,
    adoptions: Mutex<HashMap<u64, Adopt>>,
    schedulers: Mutex<HashMap<u64, TaskScheduler>>,
    sessions: Mutex<HashMap<u64, Session>>,
    strict: strict_bridge::StrictBridge,
    harnesses: Mutex<HashMap<u64, pi_durable::Harness>>,
    // Preserve the public handle returned to JS. Reacquiring it per method
    // would insert a second admission point before context reads and forks.
    conversations: Mutex<HashMap<(u64, ConversationId), pi_durable::Conversation>>,
    harness_storage: Mutex<HashMap<u64, Arc<dyn Storage>>>,
    readers: Mutex<HashMap<u64, Arc<dyn pi_durable::harness::reader::RegistryReader>>>,
    tasks: Mutex<HashMap<u64, JsonTask>>,
    runtimes: Mutex<HashMap<u64, TaskRuntime>>,
    runtime_ids: Mutex<HashMap<(u64, TaskId), u64>>,
    native_runtimes: Mutex<HashMap<u64, ErasedRuntime>>,
    tool: tool_bridge::ToolBridge,
    environment: tool_environment::EnvironmentBridge,
    builtin: builtin_bridge::BuiltinBridge,
    bound: bound_conversation_bridge::BoundBridge,
    codemode: codemode_vm_bridge::CodemodeBridge,
    host_timers: Mutex<HashMap<u64, DisarmTimer>>,
    /// Host timer callbacks with the Harness that armed them.
    host_timer_callbacks: Mutex<HashMap<u64, (u64, pi_durable::timers::TimerCallback)>>,
    transactions: Mutex<HashMap<u64, Transaction>>,
    contexts: Mutex<HashMap<u64, AbortController>>,
    phases: AtomicU64,
    source_generation_phases: AtomicU64,
    source_compaction_phases: AtomicU64,
    commits: AtomicU64,
    context_reads: AtomicU64,
    compaction_reads: AtomicU64,
    registry_reads: AtomicU64,
    agent_reads: AtomicU64,
    harness_opens: AtomicU64,
    hook_calls: AtomicU64,
    prompt: prompt_bridge::PromptBridge,
    registry: registry_bridge::RegistryBridge,
    graph: graph_bridge::GraphBridge,
    views: view_bridge::ViewBridge,
    memory: memory_bridge::MemoryBridge,
    models: model_bridge::ModelBridge,
    documents: document_bridge::DocumentBridge,
    submissions: submission_bridge::SubmissionBridge,
    inspections: AtomicU64,
    ranges: Mutex<HashMap<u64, Arc<pi_durable::harness::context::ContextRange>>>,
    context_refs: Mutex<context_bridge::ContextRefs>,
    /// The host-ack gate for native steps; see [`Hub::step_gate`].
    turns: Mutex<HostTurns>,
    turned: tokio::sync::Notify,
}
/// Frames written to the host and how many it has acknowledged after running their continuations.
/// `open` holds the tasks whose phase turn the host is pumping synchronously: their next step runs
/// as upstream's microtasks after a phase would. A host that never sends `host.ack` (the embedded
/// runtime) is not gated.
#[derive(Default)]
struct HostTurns {
    enabled: bool,
    emitted: u64,
    acked: u64,
    /// Open phase turns by runtime handle, with the runtime's task and nesting depth.
    open: HashMap<u64, (TaskId, u64)>,
    next_ticket: u64,
    /// Tickets of parked steps, released oldest first.
    parked: std::collections::BTreeSet<u64>,
}
enum Turn {
    Open(u64, TaskId),
    Close(u64),
}
impl HostTurns {
    /// Only a plain acknowledgement (after a microtask drain) enables the gate: a host that
    /// sends just the phase-turn brackets has not promised to acknowledge, and gating it would
    /// park every step until a phase turn happened to open its task.
    fn ack(&mut self, count: u64, turn: Option<Turn>, plain: bool) {
        self.enabled |= plain;
        self.acked = self.acked.max(count);
        match turn {
            Some(Turn::Open(runtime, task)) => self.open.entry(runtime).or_insert((task, 0)).1 += 1,
            Some(Turn::Close(runtime)) => {
                if let Some((_, depth)) = self.open.get_mut(&runtime) {
                    *depth -= 1;
                    if *depth == 0 {
                        self.open.remove(&runtime);
                    }
                }
            }
            None => {}
        }
    }
    /// A ticket and the frames written so far for a step that must wait, or `None` when the gate
    /// is off.
    fn park(&mut self) -> Option<(u64, u64)> {
        if !self.enabled {
            return None;
        }
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.parked.insert(ticket);
        Some((ticket, self.emitted))
    }
    /// Release a parked step when it may start: acknowledged and no older step still parked, or
    /// its own task's phase turn is open, or the host is gone.
    fn release(&mut self, ticket: u64, target: u64, task: TaskId, closed: bool) -> bool {
        let oldest = self.parked.first() == Some(&ticket);
        let turning = self.open.values().any(|(open, _)| *open == task);
        if closed || turning || (oldest && self.passes(target, task)) {
            self.parked.remove(&ticket);
            return true;
        }
        false
    }
    /// Whether a step of `task` captured at `target` frames written may start.
    fn passes(&self, target: u64, task: TaskId) -> bool {
        !self.enabled || self.acked >= target || self.open.values().any(|(open, _)| *open == task)
    }
}
#[cfg(test)]
mod host_turns_tests {
    use super::*;
    fn task(id: i64) -> TaskId {
        TaskId::from_raw(id)
    }
    #[test]
    fn steps_wait_only_once_the_host_has_promised_acknowledgements() {
        let mut turns = HostTurns {
            emitted: 3,
            ..Default::default()
        };
        // Phase-turn brackets alone do not enable the gate.
        turns.ack(0, Some(Turn::Open(9, task(1))), false);
        turns.ack(0, Some(Turn::Close(9)), false);
        assert!(turns.passes(3, task(2)));
        // A plain acknowledgement does; then a step waits for every frame before it.
        turns.ack(1, None, true);
        assert!(!turns.passes(3, task(2)));
        // An open phase turn admits only its own task.
        turns.ack(1, Some(Turn::Open(9, task(1))), false);
        assert!(turns.passes(3, task(1)) && !turns.passes(3, task(2)));
        turns.ack(1, Some(Turn::Close(9)), false);
        assert!(!turns.passes(3, task(1)));
        turns.ack(3, None, true);
        assert!(turns.passes(3, task(2)));
    }
    #[test]
    fn parked_steps_are_released_in_the_order_they_parked() {
        let mut turns = HostTurns::default();
        assert_eq!(turns.park(), None, "off until a plain acknowledgement");
        turns.ack(0, None, true);
        turns.emitted = 2;
        let (first, at) = turns.park().unwrap();
        let (second, _) = turns.park().unwrap();
        turns.ack(2, None, true);
        // Both may pass, but the younger waits for the older.
        assert!(!turns.release(second, at, task(2), false));
        assert!(turns.release(first, at, task(1), false));
        assert!(turns.release(second, at, task(2), false));
    }
}
impl Hub {
    fn emit(&self, value: JsonValue) {
        let line = value.to_string();
        let mut output = self.output.lock();
        (*output)(&line);
        self.turns.lock().emitted += 1;
        drop(output);
        self.changed.notify_waiters();
    }
    fn call(&self, method: &str, args: JsonValue) -> StorageFuture<Option<JsonValue>> {
        self.call_context(method, args, &Context::default())
    }
    /// Upstream queues a step only after the continuations of a caller it answered have run.
    /// Across the process boundary, a step waits until the host has acknowledged every frame
    /// written before it, or while a synchronous host request is open.
    fn step_gate(self: &Arc<Self>) -> pi_durable::harness::scheduler::StepGate {
        let hub = Arc::downgrade(self);
        Arc::new(move |task| {
            let hub = hub.clone();
            // The ticket keeps parked steps in the order they asked: one acknowledgement
            // releases them all, and siblings must still start as upstream queued them.
            let ticket = hub.upgrade().and_then(|hub| hub.turns.lock().park());
            Box::pin(async move {
                let (Some((ticket, target)), Some(hub)) = (ticket, hub.upgrade()) else {
                    return;
                };
                let started = std::time::Instant::now();
                let mut reported = false;
                loop {
                    let turned = hub.turned.notified();
                    tokio::pin!(turned);
                    turned.as_mut().enable();
                    let closed = hub.input_closed.load(Ordering::Acquire);
                    if hub.turns.lock().release(ticket, target, task, closed) {
                        hub.turned.notify_waiters();
                        return;
                    }
                    if reported {
                        turned.await;
                    } else if tokio::time::timeout(std::time::Duration::from_secs(1), turned)
                        .await
                        .is_err()
                    {
                        // A host that stops acknowledging parks every step; say so.
                        reported = true;
                        let turns = hub.turns.lock();
                        eprintln!(
                            "pi-durable: step of task {task} has waited {:?} for a host ack ({} of {target} frames acknowledged)",
                            started.elapsed(),
                            turns.acked
                        );
                    }
                }
            })
        })
    }
    fn host_ack(&self, value: &JsonValue) {
        let count = value["count"].as_u64().unwrap_or(0);
        let turn = match (value["open"].as_bool(), value["runtime"].as_u64()) {
            (Some(true), Some(runtime)) => self
                .runtime(value)
                .ok()
                .map(|r| Turn::Open(runtime, r.task_id().untyped())),
            (Some(false), Some(runtime)) => Some(Turn::Close(runtime)),
            _ => None,
        };
        self.turns.lock().ack(count, turn, value["open"].is_null());
        self.turned.notify_waiters();
    }
    fn call_context(
        &self,
        method: &str,
        args: JsonValue,
        context: &Context,
    ) -> StorageFuture<Option<JsonValue>> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (send, receive) = tokio::sync::oneshot::channel();
        {
            let mut pending = self.pending.lock();
            if self.input_closed.load(Ordering::Acquire) {
                return Box::pin(async { Err(Error::message("Test transport is closed")) });
            }
            pending.insert(id, Pending::Async(send));
            if method != "phase" {
                self.host_pending.lock().insert(id);
            }
        }
        self.emit(object([
            ("type", encode(&"callback")),
            ("id", encode(&id)),
            ("method", encode(&method)),
            ("args", args),
            (
                "context",
                encode(&context.value(&CALL_CONTEXT).map(|context| context.id)),
            ),
            ("abortable", encode(&context.abort_signal().is_some())),
            (
                "telemetry",
                encode(&telemetry_bridge::id(context.telemetry().as_ref())),
            ),
        ]));
        Box::pin(async move {
            receive
                .await
                .map_err(|error| Error::message(error.to_string()))?
        })
    }
    fn sync(&self, method: &str, args: JsonValue) -> Result<JsonValue> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (send, receive) = std::sync::mpsc::channel();
        {
            let mut pending = self.pending.lock();
            if self.input_closed.load(Ordering::Acquire) {
                return Err(Error::message("Test transport is closed"));
            }
            pending.insert(id, Pending::Sync(send));
        }
        self.emit(object([
            ("type", encode(&"callback")),
            ("id", encode(&id)),
            ("method", encode(&method)),
            ("args", args),
            ("sync", encode(&true)),
        ]));
        receive
            .recv()
            .map_err(|error| Error::message(error.to_string()))??
            .ok_or_else(|| Error::message("Task callback returned undefined"))
    }
    fn response(&self, value: JsonValue) {
        let id: u64 = field(&value, "id").unwrap();
        self.host_pending.lock().remove(&id);
        let result = if value["error"].is_object() {
            let error = Error::host(
                value["error"]["name"]
                    .to_utf8()
                    .unwrap_or_else(|| "Error".into()),
                value["error"]["message"]
                    .as_string()
                    .cloned()
                    .unwrap_or_else(|| "undefined".into()),
            );
            if let Some(id) = value["error"]["foreignId"].as_u64() {
                HOST_ERRORS.lock().push((error.clone(), id));
            }
            Err(error)
        } else if value["missing"].as_bool() == Some(true) {
            Ok(None)
        } else {
            Ok(Some(value["value"].clone()))
        };
        if let Some(pending) = self.pending.lock().remove(&id) {
            match pending {
                Pending::Async(send) => {
                    let _ = send.send(result);
                }
                Pending::Sync(send) => {
                    let _ = send.send(result);
                }
            }
        }
    }
    fn definition(self: &Arc<Self>, description: &JsonValue) -> Result<JsonTask> {
        let id: u64 = field(description, "id")?;
        if let Some(task) = self.tasks.lock().get(&id).cloned() {
            return Ok(task);
        }
        let phases: Vec<JsonString> = field(description, "phases")?;
        let name: JsonString = field(description, "name")?;
        let version: u64 = field(description, "version")?;
        let mut handlers = IndexMap::new();
        for phase in phases {
            let hub = self.clone();
            let name = phase.clone();
            handlers.insert(
                phase,
                Arc::new(move |task: RunningTask, runtime: TaskRuntime, context| {
                    hub.phase(id, name.clone(), task, runtime, context)
                }) as PhaseHandler<JsonValue, JsonValue, JsonValue, u64>,
            );
        }
        let hub = self.clone();
        let initial = Arc::new(move |input: &JsonValue| hub.sync("initial", encode(&(id, input))));
        let hub = self.clone();
        let abort = Arc::new(move |task, runtime, context| {
            hub.phase(id, "$abort".into(), task, runtime, context)
        }) as PhaseHandler<JsonValue, JsonValue, JsonValue, u64>;
        let migrate = if description["migrate"].as_bool() == Some(true) {
            let hub = self.clone();
            Some(Arc::new(move |input, checkpoint, version| {
                let value = hub.sync("migrate", encode(&(id, input, checkpoint, version)))?;
                Ok((value["input"].clone(), value["checkpoint"].clone()))
            })
                as pi_durable::tasks::TaskMigration<JsonValue, JsonValue>)
        } else {
            None
        };
        let task = define_task(TaskDefinition {
            name,
            version,
            initial,
            phases: handlers,
            abort,
            migrate,
            hooks: None,
        });
        self.tasks.lock().insert(id, task.clone());
        Ok(task)
    }
    fn phase(
        self: &Arc<Self>,
        definition: u64,
        phase: JsonString,
        task: RunningTask,
        runtime: TaskRuntime,
        context: Context,
    ) -> StorageFuture<()> {
        self.phases.fetch_add(1, Ordering::Relaxed);
        if task.record().info.kind.equals_str("pi.generation") {
            self.source_generation_phases
                .fetch_add(1, Ordering::Relaxed);
        }
        if task.record().info.kind.equals_str("pi.compaction") {
            self.source_compaction_phases
                .fetch_add(1, Ordering::Relaxed);
        }
        let key = (
            *context.value(&HARNESS_CONTEXT).expect("invocation Harness"),
            task.record().id(),
        );
        let candidate = self.runtime_ids.lock().get(&key).copied();
        let existing = candidate.filter(|id| {
            self.runtimes
                .lock()
                .get(id)
                .is_some_and(|held| held.same_invocation(&runtime))
        });
        let id = existing.unwrap_or_else(|| {
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            self.runtimes.lock().insert(id, runtime.clone());
            self.runtime_ids.lock().insert(key, id);
            id
        });
        if existing.is_none() {
            self.forward_runtime_signal(id, runtime.signal(), &context);
        }
        let serial = {
            let mut serials = self.phase_serials.lock();
            let serial = serials.entry(id).or_default();
            *serial += 1;
            *serial
        };
        let abort = phase.equals_str("$abort");
        let receipt_key = context
            .value(&HARNESS_CONTEXT)
            .map(|harness| (*harness, task.record().id()));
        let args = encode(&(
            definition,
            phase,
            task.record(),
            id,
            serial,
            context.value(&HARNESS_CONTEXT).map(|id| *id),
        ));
        let hub = self.clone();
        Box::pin(async move {
            if abort {
                loop {
                    let changed = hub.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    if receipt_key.is_none_or(|key| !hub.abort_receipts.lock().contains_key(&key)) {
                        break;
                    }
                    changed.await;
                }
            }
            // The caller joined the previous run before this fresh abort
            // handler. Preserve that order across the additional RPC reply.
            hub.call("phase", args).await?;
            Ok(())
        })
    }
    fn callback(
        self: &Arc<Self>,
        callback: u64,
        tx: Transaction,
        current: Option<TaskRecord>,
        request: u64,
    ) -> StorageFuture<Option<JsonValue>> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.transactions.lock().insert(id, tx);
        let operation = self.call(
            "change",
            object([
                ("callback", encode(&callback)),
                ("tx", encode(&id)),
                ("current", encode(&current)),
                ("request", encode(&request)),
            ]),
        );
        let hub = self.clone();
        Box::pin(async move {
            let result = operation.await;
            // Retained host Tx handles must reach the native settlement guard.
            let _ = hub;
            result
        })
    }
    fn created(
        self: &Arc<Self>,
        harness: u64,
        tx: Transaction,
        record: ConversationRecord,
    ) -> StorageFuture<()> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.transactions.lock().insert(id, tx);
        let call = self.call(
            "conversationCreated",
            object([
                ("harness", encode(&harness)),
                ("tx", encode(&id)),
                ("record", encode(&record)),
            ]),
        );
        let host = self.clone();
        Box::pin(async move {
            let result = call.await;
            host.transactions.lock().remove(&id);
            result?;
            Ok(())
        })
    }
    fn facade(&self, request: &JsonValue) -> Result<pi_durable::Harness> {
        self.harnesses
            .lock()
            .get(&field::<u64>(request, "harness")?)
            .cloned()
            .ok_or_else(|| Error::message("Unknown native Harness"))
    }
    fn host_set_timer(self: &Arc<Self>, harness: u64) -> SetTimer {
        let host = self.clone();
        Arc::new(move |callback, delay, background| {
            let timer = host.next.fetch_add(1, Ordering::Relaxed);
            host.host_timer_callbacks
                .lock()
                .insert(timer, (harness, callback));
            let attempt = host
                .sync(
                    "setTimer",
                    object([
                        ("harness", encode(&harness)),
                        ("timer", encode(&timer)),
                        ("delay", encode(&delay)),
                        ("background", encode(&background)),
                    ]),
                )
                .and_then(|value| decode::<Option<u64>>(&value));
            match attempt {
                Ok(Some(handle)) => {
                    let host = host.clone();
                    Some(Box::new(move || {
                        host.host_timer_callbacks.lock().remove(&timer);
                        if let Err(error) = host.sync("disarmTimer", encode(&handle)) {
                            std::panic::resume_unwind(Box::new(error));
                        }
                    }) as DisarmTimer)
                }
                Ok(None) => {
                    host.host_timer_callbacks.lock().remove(&timer);
                    None
                }
                Err(error) => {
                    host.host_timer_callbacks.lock().remove(&timer);
                    std::panic::resume_unwind(Box::new(error));
                }
            }
        })
    }
    fn conversation_options(
        self: &Arc<Self>,
        value: &JsonValue,
    ) -> Result<pi_durable::ConversationOptions> {
        let init: Option<u64> = field(value, "init")?;
        let host = self.clone();
        Ok(pi_durable::ConversationOptions {
            agent: field(value, "agent")?,
            init: init.map(|callback| {
                Arc::new(move |tx, id| host.init_conversation(Some(callback), tx, id))
                    as pi_durable::ConversationInit
            }),
        })
    }
    fn harness_session(&self, harness: u64) -> Option<Session> {
        if let Some(session) = self.sessions.lock().get(&harness).cloned() {
            return Some(session);
        }
        self.schedulers
            .lock()
            .get(&harness)
            .map(TaskScheduler::session)
    }
    /// Wait until `session` has no ready or running native work, as upstream's microtasks finish
    /// before a host timer runs. Bounded: a job that cannot finish without the host would
    /// otherwise hold the reply; reaching the bound is logged, since it should not happen.
    async fn settle(&self, session: Session, method: &str) {
        if tokio::time::timeout(std::time::Duration::from_millis(500), session.quiescent())
            .await
            .is_err()
        {
            eprintln!("pi-durable stock bridge: {method} reply did not settle within 500ms");
        }
    }
    fn scheduler(&self, request: &JsonValue) -> Result<TaskScheduler> {
        self.schedulers
            .lock()
            .get(&field::<u64>(request, "harness")?)
            .cloned()
            .ok_or_else(|| Error::message("Unknown test harness"))
    }
    fn session(&self, request: &JsonValue) -> Result<Session> {
        let id = field::<u64>(request, "harness")?;
        if let Some(session) = self.sessions.lock().get(&id).cloned() {
            return Ok(session);
        }
        Ok(self.scheduler(request)?.session())
    }
    fn transaction(&self, request: &JsonValue) -> Result<Transaction> {
        let transaction = self
            .transactions
            .lock()
            .get(&field::<u64>(request, "tx")?)
            .cloned()
            .ok_or_else(|| Error::message("Unknown transaction"))?;
        transaction.check_active()?;
        Ok(transaction)
    }
    fn forward_runtime_signal(
        self: &Arc<Self>,
        id: u64,
        signal: pi_chord::AbortSignal,
        context: &Context,
    ) {
        let harness = context.value(&HARNESS_CONTEXT).expect("invocation Harness");
        let session = self.schedulers.lock()[&*harness].session();
        let hub = self.clone();
        // Signal delivery shares native continuation order. Independent Tokio
        // jobs can reverse sibling cancellations before the host receives them.
        session.spawn(async move {
            signal.cancelled().await;
            hub.emit(object([
                ("type", encode(&"signal")),
                ("runtime", encode(&id)),
                (
                    "message",
                    encode(&signal.reason().unwrap().waiter_error().message),
                ),
            ]));
        });
    }
    fn runtime(&self, request: &JsonValue) -> Result<ErasedRuntime> {
        if let Some(runtime) = self
            .native_runtimes
            .lock()
            .get(&field::<u64>(request, "runtime")?)
            .cloned()
        {
            return Ok(runtime);
        }
        self.runtimes
            .lock()
            .get(&field::<u64>(request, "runtime")?)
            .map(NativeRuntime::erased)
            .ok_or_else(|| Error::message("Unknown test runtime"))
    }
    fn context(self: &Arc<Self>, id: u64) -> Context {
        let controller = AbortController::new();
        let context = with_abort_signal(controller.signal(), &Context::default());
        self.contexts.lock().insert(id, controller);
        with_context_value(
            &CALL_CONTEXT,
            HostContext {
                id,
                hub: Arc::downgrade(self),
            },
            &context,
        )
    }
    fn begin(
        self: Arc<Self>,
        request: JsonValue,
        context: Context,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        fn mapped<T: Serialize + Send + 'static>(
            operation: StorageFuture<T>,
        ) -> StorageFuture<Option<JsonValue>> {
            Box::pin(async move { Ok(Some(encode(&operation.await?))) })
        }
        let context = if request
            .as_object()
            .is_some_and(|fields| fields.contains_key(&JsonString::from("telemetry")))
        {
            pi_chord::context::with_telemetry(
                telemetry_bridge::context(&self, &request["telemetry"]),
                &context,
            )
        } else {
            context
        };
        let method: String = field(&request, "method")?;
        if [
            "codemode.declaration",
            "codemode.description",
            "codemode.section",
            "codemode.docCheckpoint",
        ]
        .contains(&method.as_str())
        {
            return self.codemode_metadata(request);
        }
        if method == "codemode.catalog" {
            return self.codemode_catalog(request, context);
        }
        if method == "codemode.format" {
            return self.format_codemode(request);
        }
        if method == "storage.adopt" {
            let id = field(&request, "adoption")?;
            let seq = field(&request, "seq")?;
            let adopt = self
                .adoptions
                .lock()
                .remove(&id)
                .ok_or_else(|| Error::message("Unknown or settled storage adoption"))?;
            adopt(seq);
            return Ok(Box::pin(async { Ok(None) }));
        }
        if method.starts_with("builtin.") {
            return self.builtin_request(&method, request, context);
        }
        if method.starts_with("memory.") || method.starts_with("sqlite.") {
            return self.memory_request(&method, request);
        }
        if method.starts_with("view.") {
            return self.view_request(&method, request, context);
        }
        if method.starts_with("environment.") {
            return self.environment_request(&method, request, context);
        }
        if method == "runtimeConversation" || method.starts_with("bound.") {
            return self.bound_request(&method, request, context);
        }
        if method.starts_with("tool.") {
            return self.tool_request(&method, request, context);
        }
        if method.starts_with("submission.") {
            return self.submission_request(&method, request, context);
        }
        if method.starts_with("doc.") {
            return self.document_request(&method, request, context);
        }
        if method.starts_with("graph.") {
            return self.graph_request(&method, request, context);
        }
        if method.starts_with("compaction.") {
            let value = self.compaction_request(&request);
            return Ok(Box::pin(async move { value }));
        }
        if method.starts_with("prompt.") {
            return self.prompt_request(&method, request, context);
        }
        if method.starts_with("context.") {
            return self.context_request(&method, request, context);
        }
        if method.starts_with("registry.") {
            return Ok(Box::pin(async move {
                tokio::task::spawn_blocking(move || self.registry_request(&method, request))
                    .await
                    .map_err(|error| Error::message(error.to_string()))?
            }));
        }
        if method.starts_with("session.") {
            return self.session_request(&method, request, context);
        }
        Ok(match method.as_str() {
            "transport.ready" => Box::pin(async { Ok(Some(encode(&true))) }),
            "open" => Box::pin(async move {
                let harness: u64 = field(&request, "id")?;
                let handle: u64 = field(&request, "storage")?;
                let registry = self.registry_reader(&request)?;
                self.readers.lock().insert(harness, registry.clone());
                let storage: Arc<dyn Storage> = if let Some(id) = request["nativeSqlite"].as_u64() {
                    self.memory
                        .sqlite_direct_opens
                        .fetch_add(1, Ordering::Relaxed);
                    self.sqlite_for_harness(id, handle)?
                } else if let Some(id) = request["nativeMemory"].as_u64() {
                    self.memory.direct_opens.fetch_add(1, Ordering::Relaxed);
                    self.memory_storage(id)?
                } else {
                    Arc::new(remote_storage::RemoteStorage {
                        hub: self.clone(),
                        handle,
                    })
                };
                self.harness_storage.lock().insert(harness, storage.clone());
                let mut options =
                    pi_durable::HarnessOptions::new(registry, pi_ai::models::Models::default());
                options.telemetry = telemetry_bridge::context(&self, &request["rootTelemetry"]);
                let models = Arc::downgrade(&self);
                options.model_collection = Some(Arc::new(move || {
                    models
                        .upgrade()
                        .ok_or_else(|| Error::message("Model host closed"))?
                        .host_collection(harness)
                }));
                if request["environment"].as_bool() == Some(true) {
                    let host = self.clone();
                    options.env = Some(Arc::new(move |target, context| {
                        host.build_environment(harness, target, context)
                    }));
                }
                if request["clock"].as_bool() == Some(true) {
                    let host = self.clone();
                    options.now = Some(Arc::new(move || {
                        decode(&host.sync("now", encode(&harness))?)
                    }));
                }
                if request["settings"].as_bool() == Some(true) {
                    let host = self.clone();
                    options.settings = Arc::new(move || {
                        registry_bridge::settings_input(&host.sync("settings", encode(&harness))?)
                    });
                    let host = self.clone();
                    options.document_retention_reader = Some(Arc::new(move || {
                        let value = host.sync("documentRetention", encode(&harness))?;
                        if value.is_null() {
                            return Ok(None);
                        }
                        match value.to_utf8().as_deref() {
                            Some("Infinity") => Ok(Some(f64::INFINITY)),
                            Some("-Infinity") => Ok(Some(f64::NEG_INFINITY)),
                            Some("NaN") => Ok(Some(f64::NAN)),
                            _ => decode(&value).map(Some),
                        }
                    }));
                }
                options.default_document_retention_ms = field(&request, "retention")?;
                options.read_only = request["readOnly"].as_bool() == Some(true);
                if request["setTimer"].as_bool() == Some(true) {
                    options.set_timer = Some(self.host_set_timer(harness));
                }
                if request["onWake"].as_bool() == Some(true) {
                    let host = self.clone();
                    options.on_wake = Some(Arc::new(move |wake| {
                        let mut value = object([("active", encode(&wake.active))]);
                        if let Some(at) = wake.at {
                            value
                                .as_object_mut()
                                .unwrap()
                                .insert("at".into(), encode(&at));
                        }
                        host.sync(
                            "onWake",
                            object([("harness", encode(&harness)), ("wake", value)]),
                        )?;
                        Ok(())
                    }));
                }
                if request["conversationCreated"].as_bool() == Some(true) {
                    let host = self.clone();
                    options.conversation_created = Some(Arc::new(move |tx, record| {
                        host.created(harness, tx, record)
                    }));
                }
                options.step_gate = Some(self.step_gate());
                let host = self.clone();
                options.on_report = Some(Arc::new(move |error, info| {
                    host.emit(object([
                        ("type", encode(&"report")),
                        ("harness", encode(&harness)),
                        ("error", error_value(&error)),
                        ("info", encode(&info)),
                    ]))
                }));
                let facade = pi_durable::Harness::open(
                    storage,
                    options,
                    &with_context_value(&HARNESS_CONTEXT, harness, &context),
                )
                .await?;
                let scheduler = facade.scheduler().clone();
                self.submissions
                    .services
                    .lock()
                    .insert(harness, facade.submissions().clone());
                self.harnesses.lock().insert(harness, facade);
                self.harness_opens.fetch_add(1, Ordering::Relaxed);
                self.schedulers.lock().insert(harness, scheduler);
                Ok(Some(encode(&harness)))
            }),
            "root" => {
                let harness = field(&request, "harness")?;
                let root = self
                    .facade(&request)?
                    .root(self.conversation_options(&request["options"])?, &context);
                let hub = self.clone();
                Box::pin(async move {
                    let conversation = root.await?;
                    let id = conversation.id;
                    hub.conversations.lock().insert((harness, id), conversation);
                    Ok(Some(encode(&id)))
                })
            }
            "wake" => {
                let wake = self.scheduler(&request)?.wake();
                let mut value = object([("active", encode(&wake.active))]);
                if let Some(at) = wake.at {
                    value
                        .as_object_mut()
                        .unwrap()
                        .insert("at".into(), encode(&at));
                }
                Box::pin(async move { Ok(Some(value)) })
            }
            "inspect" => {
                self.inspections.fetch_add(1, Ordering::Relaxed);
                mapped(self.scheduler(&request)?.inspect(&context))
            }
            "conversationAgent" => {
                let agent = self.facade(&request)?.resolve_agent(
                    field(&request, "conversation")?,
                    None,
                    &context,
                );
                let host = self.clone();
                Box::pin(async move {
                    Ok(Some(host.encode_agent(agent.await?.typed::<registry_bridge::ToolHandle, registry_bridge::SectionHandle>()?)))
                })
            }
            "conversation" => {
                let harness = field(&request, "harness")?;
                let id = field(&request, "conversation")?;
                let read = self.facade(&request)?.conversation(id, &context);
                let hub = self.clone();
                Box::pin(async move {
                    Ok(read.await?.map(|conversation| {
                        hub.conversations.lock().insert((harness, id), conversation);
                        object([("id", encode(&id))])
                    }))
                })
            }
            "conversation.abort" => {
                let scheduler = self.scheduler(&request)?;
                scheduler.resume()?;
                mapped(scheduler.abort_conversation(
                    field(&request, "conversation")?,
                    request["background"].as_bool().unwrap_or(false),
                    &context,
                ))
            }
            "tx.task" => {
                let operation = self.transaction(&request)?.task(field(&request, "task")?);
                Box::pin(async move { Ok(operation.await?.map(|task| encode(&task))) })
            }
            "tx.setTask" => {
                self.transaction(&request)?
                    .set_task(field(&request, "record")?)?;
                Box::pin(async { Ok(None) })
            }
            "placeSubmission" => {
                self.transaction(&request)?
                    .place_submission(field(&request, "submission")?, field(&request, "entry")?)?;
                Box::pin(async { Ok(None) })
            }
            "tx.createConversation" | "tx.forkConversation" => {
                let ownership = match request["options"]["ownership"]["kind"].to_utf8().as_deref() {
                    Some("ownerless") => ConversationOwnership::Ownerless,
                    Some("task") => ConversationOwnership::Task(field(
                        &request["options"]["ownership"],
                        "taskId",
                    )?),
                    _ => return Err(Error::message("Unknown conversation ownership")),
                };
                let tx = self.transaction(&request)?;
                if method == "tx.createConversation" {
                    Box::pin(async move {
                        let record = tx.create_conversation(ownership).await?;
                        if request["options"]["agent"].is_object() {
                            pi_durable::harness::agent::configure(
                                &tx,
                                record.id,
                                field(&request["options"], "agent")?,
                            )
                            .await?;
                        }
                        Ok(Some(encode(&record)))
                    })
                } else {
                    mapped(tx.fork_conversation(
                        field(&request, "conversation")?,
                        field(&request, "at")?,
                        ownership,
                    ))
                }
            }
            "createConversation" | "forkConversation" => {
                let harness = field(&request, "harness")?;
                let facade = self.facade(&request)?;
                let ownership = match request["options"]["ownership"]["kind"].to_utf8().as_deref() {
                    Some("ownerless") => ConversationOwnership::Ownerless,
                    Some("task") => ConversationOwnership::Task(field(
                        &request["options"]["ownership"],
                        "taskId",
                    )?),
                    _ => return Err(Error::message("Unknown conversation ownership")),
                };
                let options = self.conversation_options(&request["options"])?;
                let options = pi_durable::ConversationCreateOptions {
                    ownership,
                    agent: options.agent,
                    init: options.init,
                };
                let operation = if method == "createConversation" {
                    facade.create_conversation(options, &context)
                } else {
                    let conversation = self
                        .conversations
                        .lock()
                        .get(&(harness, field(&request, "conversation")?))
                        .cloned()
                        .ok_or_else(|| Error::message("Unknown conversation handle"))?;
                    let at = field(&request, "at")?;
                    conversation.fork(at, options, &context)
                };
                let hub = self.clone();
                Box::pin(async move {
                    let conversation = operation.await?;
                    let id = conversation.id;
                    hub.conversations.lock().insert((harness, id), conversation);
                    Ok(Some(encode(&id)))
                })
            }
            "usage" => mapped(self.facade(&request)?.usage(&context)),
            "retryTask" => mapped(
                self.facade(&request)?
                    .retry_task(field(&request, "task")?, &context),
            ),
            "conversation.compact" => {
                mapped(pi_durable::harness::compaction::compact_conversation(
                    &self.scheduler(&request)?,
                    field(&request, "conversation")?,
                    field(&request, "instructions")?,
                    &context,
                ))
            }
            "waitForIdle" => {
                let scheduler = self.scheduler(&request)?;
                scheduler.resume()?;
                let operation = scheduler.wait_for_idle(field(&request, "conversation")?, &context);
                let hub = self.clone();
                Box::pin(async move {
                    operation.await?;
                    // Idleness settles (retention timers armed) in the same turn as upstream.
                    hub.settle(scheduler.session(), "waitForIdle").await;
                    Ok(Some(encode(&())))
                })
            }
            "appendEntry" => {
                let tx = self.transaction(&request)?;
                let draft = &request["entry"];
                let mut entry = EntryDraft::new(field::<JsonString>(draft, "kind")?);
                entry.model = field(draft, "model")?;
                entry.head = if draft["head"].is_null() {
                    None
                } else if draft["head"].string_equals("self") {
                    Some(EntryHead::ThisEntry)
                } else {
                    Some(EntryHead::Entry(field(draft, "head")?))
                };
                entry.edits = field(draft, "edits")?;
                entry.data = draft
                    .as_object()
                    .and_then(|draft| draft.get(&JsonString::from("data")))
                    .cloned();
                mapped(tx.append_entry(field(&request, "conversation")?, entry))
            }
            "tx.conversation" => mapped(
                self.transaction(&request)?
                    .conversation(field(&request, "conversation")?),
            ),
            "scanConversations" => mapped(self.transaction(&request)?.scan_conversations(
                field(&request, "query")?,
                field(&request, "limit")?,
                field(&request, "cursor")?,
            )),
            "scanTasks" => mapped(self.transaction(&request)?.scan_tasks(
                field(&request, "query")?,
                field(&request, "limit")?,
                field(&request, "cursor")?,
            )),
            "tx.latestHeadMarker" => {
                let operation = self
                    .transaction(&request)?
                    .latest_head_marker(field(&request, "conversation")?);
                Box::pin(async move { Ok(operation.await?.map(|entry| encode(&entry))) })
            }
            "entry" => {
                let operation = self.transaction(&request)?.entry(field(&request, "entry")?);
                let kind: Option<JsonString> = field(&request, "kind")?;
                Box::pin(async move {
                    Ok(operation
                        .await?
                        .filter(|entry| kind.as_ref().is_none_or(|kind| entry.kind == *kind))
                        .map(|entry| encode(&entry)))
                })
            }
            "createSubmission" => {
                let operation = self
                    .transaction(&request)?
                    .create_submission(field(&request, "record")?);
                Box::pin(async move { Ok(Some(encode(&operation.await?))) })
            }
            "settleSubmission" => {
                self.transaction(&request)?.settle_submission(
                    field(&request, "submission")?,
                    field(&request, "settlement")?,
                )?;
                Box::pin(async { Ok(None) })
            }
            "scanEntries" => {
                let operation = self.transaction(&request)?.scan_entries(
                    field(&request, "query")?,
                    field(&request, "limit")?,
                    field(&request, "cursor")?,
                );
                Box::pin(async move { Ok(Some(encode(&operation.await?))) })
            }
            "entries" => {
                let scheduler = self.scheduler(&request)?;
                let mut query = request["query"].clone();
                query
                    .as_object_mut()
                    .unwrap()
                    .insert("conversationId".into(), request["conversation"].clone());
                let query: EntryQuery = decode(&query)?;
                let limit = field(&request, "limit")?;
                let cursor = field(&request, "cursor")?;
                let operation = scheduler.session().commit(
                    move |tx| async move { tx.scan_entries(query, limit, cursor).await },
                    &context,
                );
                Box::pin(async move { Ok(Some(encode(&operation.await?))) })
            }
            "runtimeContext" => {
                self.context_reads.fetch_add(1, Ordering::Relaxed);
                let read = self.runtime(&request)?.context(
                    field(&request, "conversation")?,
                    field(&request, "at")?,
                    &context,
                );
                // `aliased`: keep message identity across `messages` and `contributions`; `refs`:
                // send values the host already holds as references.
                if request["refs"].as_bool() == Some(true) {
                    let hub = self.clone();
                    let namespace = request["harness"].as_u64().unwrap_or_default();
                    Box::pin(async move {
                        Ok(Some(context_bridge::encode_view_refs(
                            &read.await?,
                            namespace,
                            &hub.context_refs,
                        )))
                    })
                } else if request["aliased"].as_bool() == Some(true) {
                    Box::pin(async move { Ok(Some(context_bridge::encode_view(&read.await?))) })
                } else {
                    mapped(read)
                }
            }
            "conversationContext" => {
                self.context_reads.fetch_add(1, Ordering::Relaxed);
                let harness = field(&request, "harness")?;
                let conversation = self
                    .conversations
                    .lock()
                    .get(&(harness, field(&request, "conversation")?))
                    .cloned()
                    .ok_or_else(|| Error::message("Unknown conversation handle"))?;
                let read = conversation.context(field(&request, "at")?, &context);
                if request["refs"].as_bool() == Some(true) {
                    let hub = self.clone();
                    let namespace = request["harness"].as_u64().unwrap_or_default();
                    Box::pin(async move {
                        Ok(Some(context_bridge::encode_view_refs(
                            &read.await?,
                            namespace,
                            &hub.context_refs,
                        )))
                    })
                } else if request["aliased"].as_bool() == Some(true) {
                    Box::pin(async move { Ok(Some(context_bridge::encode_view(&read.await?))) })
                } else {
                    mapped(read)
                }
            }
            "phase.enter" => {
                let active = !self.runtime(&request)?.signal().aborted();
                Box::pin(async move { Ok(Some(encode(&active))) })
            }
            "host.turn" => Box::pin(async { Ok(None) }),
            // A host forgets the context values it kept for a Harness (see `ContextRefs`).
            "contextRefs.forget" => {
                self.context_refs
                    .lock()
                    .forget(field(&request, "namespace")?);
                Box::pin(async { Ok(None) })
            }
            "change.turn" => {
                let request: u64 = field(&request, "request")?;
                Box::pin(async move {
                    loop {
                        let changed = self.changed.notified();
                        tokio::pin!(changed);
                        changed.as_mut().enable();
                        if !self.contexts.lock().contains_key(&request)
                            || !self.host_pending.lock().is_empty()
                        {
                            break;
                        }
                        changed.await;
                    }
                    Ok(None)
                })
            }
            "phase.turn" => {
                let runtime = self.runtime(&request)?;
                let id: u64 = field(&request, "runtime")?;
                let serial: u64 = field(&request, "serial")?;
                let owner = self.session(&request)?;
                Box::pin(async move {
                    loop {
                        let changed = self.changed.notified();
                        tokio::pin!(changed);
                        changed.as_mut().enable();
                        // The source continues its synchronous/microtask work
                        // before a host timer. Yield when the invocation ends,
                        // another phase starts, or a host promise can block it.
                        if runtime.signal().aborted()
                            || self
                                .phase_serials
                                .lock()
                                .get(&id)
                                .is_some_and(|current| *current > serial)
                            || !self.host_pending.lock().is_empty()
                        {
                            break;
                        }
                        changed.await;
                    }
                    // Cleanup may have queued wake delivery behind this turn.
                    // Drain that cooperative queue without joining storage or a
                    // crashed invocation whose backend can intentionally hang.
                    // Settling the invocation (outcome commit, waiter wakes) may take several
                    // cooperative hops; keep draining while they still produce frames, so a waiter
                    // the host is about to observe is answered before the host's next timer.
                    for _ in 0..32 {
                        let before = self.turns.lock().emitted;
                        let (send, receive) = tokio::sync::oneshot::channel();
                        owner.spawn(async move {
                            let _ = send.send(());
                        });
                        receive
                            .await
                            .map_err(|error| Error::message(error.to_string()))?;
                        if self.turns.lock().emitted == before {
                            break;
                        }
                    }
                    Ok(None)
                })
            }
            "hostTimerFire" => {
                let host = self.clone();
                let timer = field(&request, "timer")?;
                Box::pin(async move {
                    let callback = host.host_timer_callbacks.lock().remove(&timer);
                    if let Some((harness, callback)) = callback {
                        callback();
                        // A host timer callback runs to completion, its microtasks included, before
                        // the host's next check (re-arming timers the callback disarmed).
                        if let Some(session) = host.harness_session(harness) {
                            host.settle(session, "hostTimerFire").await;
                        }
                    }
                    Ok(None)
                })
            }
            "runtimeTimer" => {
                let runtime = self.runtime(&request)?;
                let callback: u64 = field(&request, "callback")?;
                let delay = field(&request, "delay")?;
                let timer = self.next.fetch_add(1, Ordering::Relaxed);
                let hub = self.clone();
                // Host timer registration may call back synchronously. Keep
                // stdin free to deliver its response while this operation runs.
                Box::pin(async move {
                    let host = hub.clone();
                    let disarm = runtime.set_timer(
                        move || {
                            let operation = host.call("timerFire", encode(&callback));
                            tokio::spawn(async move {
                                let _ = operation.await;
                            });
                        },
                        delay,
                    );
                    hub.host_timers.lock().insert(timer, disarm);
                    Ok(Some(encode(&timer)))
                })
            }
            "runtimeDisarmTimer" => {
                let host = self.clone();
                let timer = field(&request, "timer")?;
                Box::pin(async move {
                    let disarm = host.host_timers.lock().remove(&timer);
                    if let Some(disarm) = disarm {
                        disarm();
                    }
                    Ok(None)
                })
            }
            "runtimeEnv" => {
                let operation = self.runtime(&request)?.env(&context);
                let hub = self.clone();
                Box::pin(async move {
                    Ok(operation
                        .await?
                        .and_then(|env| hub.environment.handle(&env))
                        .map(|id| encode(&id)))
                })
            }
            "runtimeSettings" => {
                let runtime = self.runtime(&request)?;
                Box::pin(async move {
                    tokio::task::spawn_blocking(move || {
                        Ok(Some(registry_bridge::settings_value(&runtime.settings()?)))
                    })
                    .await
                    .map_err(|error| Error::message(error.to_string()))?
                })
            }
            "runtimeEntry" => {
                let operation = self
                    .runtime(&request)?
                    .entry(field(&request, "entry")?, &context);
                Box::pin(async move {
                    Ok(operation
                        .await?
                        .filter(|entry| {
                            request["kind"].is_null()
                                || entry.kind == field::<JsonString>(&request, "kind").unwrap()
                        })
                        .map(|entry| encode(&entry)))
                })
            }
            "runtimeNow" | "runtimeReport" => {
                let runtime = self.runtime(&request)?;
                Box::pin(async move {
                    tokio::task::spawn_blocking(move || {
                        if method == "runtimeNow" {
                            Ok(Some(encode(&runtime.now()?)))
                        } else {
                            let error = Error::host(
                                field::<String>(&request["error"], "name")?,
                                field::<JsonString>(&request["error"], "message")?,
                            );
                            if let Some(id) = request["error"]["foreignId"].as_u64() {
                                HOST_ERRORS.lock().push((error.clone(), id));
                            }
                            match request["scope"]["type"].to_utf8().as_deref() {
                                Some("extension") => runtime.report_extension(
                                    error,
                                    field(&request["scope"], "extension")?,
                                )?,
                                Some("host") => {
                                    let callback =
                                        match request["scope"]["callback"].to_utf8().as_deref() {
                                            Some("models") => "models",
                                            Some("settings") => "settings",
                                            Some("env") => "env",
                                            Some("now") => "now",
                                            Some("setTimer") => "setTimer",
                                            Some("registry") => "registry",
                                            Some("onWake") => "onWake",
                                            _ => {
                                                return Err(Error::message(
                                                    "Unknown host report callback",
                                                ));
                                            }
                                        };
                                    runtime.report_host(error, callback)?;
                                }
                                _ => runtime.report(error)?,
                            }
                            Ok(None)
                        }
                    })
                    .await
                    .map_err(|error| Error::message(error.to_string()))?
                })
            }
            "sleepUntil" => mapped(
                self.runtime(&request)?.sleep_until(
                    match request["until"]
                        .as_string()
                        .and_then(|value| value.to_string_checked().ok())
                        .as_deref()
                    {
                        Some("NaN") => f64::NAN,
                        Some("Infinity") => f64::INFINITY,
                        Some("-Infinity") => f64::NEG_INFINITY,
                        _ => field(&request, "until")?,
                    },
                    field(&request, "checkpoint")?,
                    &context,
                ),
            ),
            "runtimeAgent" => {
                self.agent_reads.fetch_add(1, Ordering::Relaxed);
                let operation = self.runtime(&request)?.agent(&context);
                let hub = self.clone();
                Box::pin(async move {
                    let agent = operation
                        .await?
                        .typed::<registry_bridge::ToolHandle, registry_bridge::SectionHandle>()?;
                    Ok(Some(hub.encode_agent(agent)))
                })
            }
            "runtimeHooks" => {
                let callback: u64 = field(&request, "callback")?;
                let name: JsonString = field(&request, "name")?;
                let lookup = self.clone();
                let invoke = self.clone();
                let key = pi_durable::harness::hooks::HookKey::try_new(
                    name.clone(),
                    move |handlers: Arc<u64>| {
                        decode::<Option<u64>>(&lookup.sync(
                            "hookMember",
                            object([
                                ("handlers", encode(handlers.as_ref())),
                                ("name", encode(&name)),
                            ]),
                        )?)
                    },
                );
                mapped(
                    self.runtimes
                        .lock()
                        .get(&field::<u64>(&request, "runtime")?)
                        .cloned()
                        .ok_or_else(|| Error::message("No generic hooks binding"))?
                        .hooks()
                        .each(&key, move |handler| {
                            invoke.hook_calls.fetch_add(1, Ordering::Relaxed);
                            let call = invoke.call(
                                "hookInvoke",
                                object([
                                    ("callback", encode(&callback)),
                                    ("handler", encode(&handler)),
                                ]),
                            );
                            async move {
                                call.await?;
                                Ok(())
                            }
                        }),
                )
            }
            "configure" => {
                let scheduler = self.scheduler(&request)?;
                let conversation = field(&request, "conversation")?;
                let change = field(&request, "change")?;
                mapped(scheduler.session().commit(
                    move |tx| async move {
                        pi_durable::harness::agent::configure(&tx, conversation, change).await
                    },
                    &context,
                ))
            }
            "runtimeRegistry" => {
                let snapshot = self.runtime_registry(&request)?;
                Box::pin(async move { Ok(Some(encode(&snapshot))) })
            }
            "resume" => {
                self.scheduler(&request)?.resume()?;
                Box::pin(async { Ok(None) })
            }
            "close" => mapped(self.session(&request)?.close(&context)),
            "unloadDocuments" => mapped(self.session(&request)?.unload_documents()),
            "closed" => {
                let session = self.session(&request)?;
                Box::pin(async move {
                    Ok(Some(match session.closed().await {
                        SessionEnd::Closed => object([("reason", encode(&"closed"))]),
                        SessionEnd::Failed(error) => object([
                            ("reason", encode(&"failed")),
                            ("error", error_value(&error)),
                        ]),
                    }))
                })
            }
            "getTask" => {
                let id = field(&request, "task")?;
                let scheduler = self.scheduler(&request)?;
                let operation = scheduler
                    .session()
                    .commit(move |tx| async move { tx.task(id).await }, &context);
                Box::pin(async move { Ok(operation.await?.map(|record| encode(&record))) })
            }
            "abortTask" => mapped(
                self.scheduler(&request)?
                    .abort(field(&request, "task")?, &context),
            ),
            "waitForTask" => {
                let scheduler = self.scheduler(&request)?;
                scheduler.resume()?;
                mapped(scheduler.wait_for_task(field(&request, "task")?, &context))
            }
            "runtimeOwnedTasks" => mapped(self.runtime(&request)?.owned_tasks(&context)),
            "runtimeWaitForTask" => mapped(self.runtime(&request)?.wait_for_task(
                TypedTaskId::<JsonValue>::from_untyped(field(&request, "task")?),
                &context,
            )),
            "runtimeGetTask" => {
                let runtime = self.runtime(&request)?;
                let operation = runtime.get_task(
                    TypedTaskId::<JsonValue>::from_untyped(field(&request, "task")?),
                    &context,
                );
                Box::pin(async move { Ok(operation.await?.map(|record| encode(&record))) })
            }
            "outcomes" => {
                let ids: Vec<TaskId> = field(&request, "tasks")?;
                mapped(
                    self.runtime(&request)?.outcomes(
                        &ids.into_iter()
                            .map(TypedTaskId::<JsonValue>::from_untyped)
                            .collect::<Vec<_>>(),
                        &context,
                    ),
                )
            }
            "memo" => {
                let runtime = self.runtime(&request)?;
                let name: JsonString = field(&request, "name")?;
                if let Some(candidate) = request
                    .as_object()
                    .unwrap()
                    .get(&JsonString::from("candidate"))
                {
                    mapped(runtime.store_memo(name, candidate.clone(), &context))
                } else {
                    runtime.memo::<JsonValue>(name, &context)
                }
            }
            "commit" => {
                self.commits.fetch_add(1, Ordering::Relaxed);
                let callback = field(&request, "callback")?;
                let request_id = field(&request, "id")?;
                if request
                    .as_object()
                    .unwrap()
                    .contains_key(&JsonString::from("runtime"))
                {
                    let runtime = self.runtime(&request)?;
                    let hub = self.clone();
                    mapped(runtime.commit(
                        move |tx, current| async move {
                            hub.callback(callback, tx, Some(current.into_record()), request_id)
                                .await?
                                .map(|value| to_next(decode(&value)?))
                                .transpose()
                        },
                        &context,
                    ))
                } else {
                    let session = self.session(&request)?;
                    let conversation = field(&request, "conversation")?;
                    let hub = self.clone();
                    session.commit_scoped(
                        move |tx| async move { hub.callback(callback, tx, None, request_id).await },
                        &context,
                        TransactionScope {
                            conversation_id: conversation,
                            task_id: None,
                        },
                    )
                }
            }
            "createTask" => {
                let tx = self
                    .transactions
                    .lock()
                    .get(&field::<u64>(&request, "tx")?)
                    .cloned()
                    .ok_or_else(|| Error::message("Unknown transaction"))?;
                let options = &request["options"];
                let ownership = task_ownership(options)?;
                let task = self.stock_catalog_task(&request["definition"])?;
                let mut create = TaskOptions::new(ownership);
                create.conversation_id = options
                    .as_object()
                    .unwrap()
                    .get(&JsonString::from("conversationId"))
                    .map(decode)
                    .transpose()?;
                create.trace = if options["trace"].string_equals("root") {
                    Some(TaskTrace::Root)
                } else if options["trace"].is_object() {
                    Some(TaskTrace::Carrier(field(options, "trace")?))
                } else {
                    None
                };
                create.links = field(options, "links")?;
                create.submission_ids = field(options, "submissionIds")?;
                create.background = options["background"].as_bool().unwrap_or(false);
                create.abandon_on_restart = options["abandonOnRestart"].as_bool().unwrap_or(false);
                mapped(tx.create_erased_task(&task, request["input"].clone(), create))
            }
            "indexSizes" => {
                let sizes = self.scheduler(&request)?.index_sizes();
                Box::pin(async move { Ok(Some(encode(&sizes))) })
            }
            "diagnostics" => {
                let schedulers = self
                    .schedulers
                    .lock()
                    .iter()
                    .map(|(id, scheduler)| (*id, scheduler.clone()))
                    .collect::<Vec<_>>();
                Box::pin(async move {
                    let mut states = Vec::new();
                    for (id, scheduler) in schedulers {
                        let inspection = scheduler.inspect(&Context::default()).await;
                        let wake = scheduler.wake();
                        states.push(object([
                            ("harness", encode(&id)),
                            ("now", encode(&native_now())),
                            (
                                "wake",
                                object([
                                    ("active", encode(&wake.active)),
                                    ("at", encode(&wake.at)),
                                ]),
                            ),
                            (
                                "inspection",
                                inspection
                                    .map(|value| encode(&value))
                                    .unwrap_or_else(|error| error_value(&error)),
                            ),
                        ]));
                    }
                    Ok(Some(encode(&states)))
                })
            }
            "stats" => {
                let mut value = encode(
                    &serde_json::json!({"nativeMemoryOpens":self.memory.direct_opens.load(Ordering::Relaxed),"nativeMemoryCalls":self.memory.calls.load(Ordering::Relaxed),"nativeMemoryCommits":self.memory.commits.load(Ordering::Relaxed),"nativeModelRequests":self.models.requests.load(Ordering::Relaxed),"nativeModelCompletions":self.models.completions.load(Ordering::Relaxed),"nativeCompactionHooks":self.models.compaction_hooks.load(Ordering::Relaxed),"sourceCompactionPhases":self.source_compaction_phases.load(Ordering::Relaxed),"nativeGenerationHooks":self.models.hook_calls.load(Ordering::Relaxed),"nativeModelPolls":self.models.polls.load(Ordering::Relaxed),"nativeModelCancels":self.models.cancellations.load(Ordering::Relaxed),"nativeEventFrames":self.views.event_frames.load(Ordering::Relaxed),"nativeEventReads":self.views.event_reads.load(Ordering::Relaxed),"viewReads":self.views.reads.load(Ordering::Relaxed),"nativeToolHooks":self.tool.hook_calls.load(Ordering::Relaxed),"nativeToolCalls":self.tool.executions.load(Ordering::Relaxed),"compactionReads":self.compaction_reads.load(Ordering::Relaxed),"promptReplays":self.prompt.replays.load(Ordering::Relaxed),"promptPlans":self.prompt.plans.load(Ordering::Relaxed),"promptRenders":self.prompt.renders.load(Ordering::Relaxed),"promptSectionCalls":self.prompt.section_calls.load(Ordering::Relaxed),"phases":self.phases.load(Ordering::Relaxed),"sourceGenerationPhases":self.source_generation_phases.load(Ordering::Relaxed),"commits":self.commits.load(Ordering::Relaxed),"contextReads":self.context_reads.load(Ordering::Relaxed),"registryReads":self.registry_reads.load(Ordering::Relaxed),"agentReads":self.agent_reads.load(Ordering::Relaxed),"nativeHarnessOpens":self.harness_opens.load(Ordering::Relaxed),"hookCalls":self.hook_calls.load(Ordering::Relaxed),"graphReads":self.graph.reads.load(Ordering::Relaxed),"graphFrames":self.graph.frames.load(Ordering::Relaxed),"inspections":self.inspections.load(Ordering::Relaxed),"readerSnapshots":self.registry.reader_snapshots.load(Ordering::Relaxed),"readerSubscriptions":self.registry.reader_subscriptions.load(Ordering::Relaxed),"documentReads":self.documents.reads.load(Ordering::Relaxed),"documentWrites":self.documents.writes.load(Ordering::Relaxed),"documentWatches":self.documents.watches.load(Ordering::Relaxed),"documentFrames":self.documents.watch_frames.load(Ordering::Relaxed),"submissionAdmissions":self.submissions.admissions.load(Ordering::Relaxed),"submissionReads":self.submissions.reads.load(Ordering::Relaxed),"submissionWaits":self.submissions.waits.load(Ordering::Relaxed),"submissionAborts":self.submissions.aborts.load(Ordering::Relaxed)}),
                );
                value.as_object_mut().unwrap().extend([
                    (
                        "nativeSqliteOpens".into(),
                        encode(&self.memory.sqlite_opens.load(Ordering::Relaxed)),
                    ),
                    (
                        "nativeSqliteDirectOpens".into(),
                        encode(&self.memory.sqlite_direct_opens.load(Ordering::Relaxed)),
                    ),
                    (
                        "nativeSqliteCalls".into(),
                        encode(&self.memory.sqlite_calls.load(Ordering::Relaxed)),
                    ),
                    (
                        "nativeSqliteCommits".into(),
                        encode(&self.memory.sqlite_commits.load(Ordering::Relaxed)),
                    ),
                ]);
                value.as_object_mut().unwrap().insert(
                    "nativeSessionOpens".into(),
                    encode(&self.sessions.lock().len()),
                );
                value.as_object_mut().unwrap().insert(
                    "nativeSearchCalls".into(),
                    encode(&self.tool.searches.load(Ordering::Relaxed)),
                );
                value.as_object_mut().unwrap().insert(
                    "nativeCodemodeFormats".into(),
                    encode(&self.tool.codemode_formats.load(Ordering::Relaxed)),
                );
                value.as_object_mut().unwrap().insert(
                    "nativeModelEffects".into(),
                    encode(&self.tool.model_effects.load(Ordering::Relaxed)),
                );
                value.as_object_mut().unwrap().insert(
                    "nativeTypedRequests".into(),
                    encode(&self.tool.typed_requests.load(Ordering::Relaxed)),
                );
                value.as_object_mut().unwrap().insert(
                    "nativeCatalogQueries".into(),
                    encode(&self.tool.catalog_queries.load(Ordering::Relaxed)),
                );
                value.as_object_mut().unwrap().insert(
                    "nativeCodemodeExecutions".into(),
                    encode(&self.codemode.executions.load(Ordering::Relaxed)),
                );
                value.as_object_mut().unwrap().insert(
                    "nativeVmStarts".into(),
                    encode(&self.codemode.starts.load(Ordering::Relaxed)),
                );
                value.as_object_mut().unwrap().insert(
                    "nativeVmDeliveries".into(),
                    encode(&self.codemode.deliveries.load(Ordering::Relaxed)),
                );
                value.as_object_mut().unwrap().insert(
                    "nativeCodemodeGlobals".into(),
                    encode(&self.codemode.globals.load(Ordering::Relaxed)),
                );
                value.as_object_mut().unwrap().insert(
                    "nativeCodemodeCheckpointPolicies".into(),
                    encode(&self.codemode.checkpoint_policies.load(Ordering::Relaxed)),
                );
                Box::pin(async move { Ok(Some(value)) })
            }
            other => {
                return Err(Error::message(format!(
                    "Unknown test bridge method {other}"
                )));
            }
        })
    }
}
fn to_next(state: TaskState) -> Result<NextTaskState> {
    Ok(match state {
        TaskState::Running { checkpoint, .. } => NextTaskState::Running { checkpoint },
        TaskState::Waiting {
            checkpoint,
            on,
            policy,
            ..
        } => NextTaskState::Waiting {
            checkpoint,
            on,
            policy,
        },
        TaskState::Sleeping {
            checkpoint, until, ..
        } => NextTaskState::Sleeping { checkpoint, until },
        TaskState::Terminal { outcome } => NextTaskState::Terminal { outcome },
        _ => return Err(Error::message("Not a task-owned next state")),
    })
}
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("--stock-base64") {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes).unwrap();
        let encoded = pi_durable::tools::image_processor::to_base64(&bytes);
        std::io::stdout().write_all(encoded.as_bytes()).unwrap();
        if let Some(marker) = std::env::var_os("PI_DURABLE_NATIVE_BASE64_MARKER") {
            std::fs::write(marker, bytes.len().to_string()).unwrap();
        }
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--order-tool-results") {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).unwrap();
        let messages: Vec<Arc<JsonValue>> = serde_json::from_str(&input).unwrap();
        println!(
            "{}",
            encode(&pi_durable::harness::context::order_tool_results(&messages))
        );
        return;
    }
    let hub = new_hub(Box::new(|line: &str| {
        let mut output = std::io::stdout().lock();
        writeln!(output, "{line}").unwrap();
        output.flush().unwrap();
    }));
    // Offer the host-ack step gate; a host that never acknowledges is not gated.
    hub.emit(object([("type", encode(&"host.acks"))]));
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let (send, lane) = spawn_lane(&hub, &runtime);
        for line in std::io::stdin().lock().lines() {
            let value: JsonValue = serde_json::from_str(&line.unwrap()).unwrap();
            receive(&hub, &runtime, &send, value);
        }
        close_input(&hub);
        drop(send);
        lane.join().unwrap();
    })
    .await
    .unwrap();
}
/// A Hub writing its frames to `output`.
pub(crate) fn new_hub(output: Output) -> Arc<Hub> {
    Arc::new(Hub {
        output: Mutex::new(output),
        next: AtomicU64::new(1_000_000),
        pending: Mutex::new(HashMap::new()),
        host_pending: Mutex::new(HashSet::new()),
        phase_serials: Mutex::new(HashMap::new()),
        abort_receipts: Mutex::new(HashMap::new()),
        changed: tokio::sync::Notify::new(),
        input_closed: AtomicBool::new(false),
        adoptions: Mutex::new(HashMap::new()),
        schedulers: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        strict: strict_bridge::StrictBridge::default(),
        harnesses: Mutex::new(HashMap::new()),
        conversations: Mutex::new(HashMap::new()),
        harness_storage: Mutex::new(HashMap::new()),
        readers: Mutex::new(HashMap::new()),
        tasks: Mutex::new(HashMap::new()),
        runtimes: Mutex::new(HashMap::new()),
        runtime_ids: Mutex::new(HashMap::new()),
        native_runtimes: Mutex::new(HashMap::new()),
        tool: tool_bridge::ToolBridge::default(),
        environment: tool_environment::EnvironmentBridge::default(),
        builtin: builtin_bridge::BuiltinBridge::default(),
        bound: bound_conversation_bridge::BoundBridge::default(),
        codemode: codemode_vm_bridge::CodemodeBridge::default(),
        host_timers: Mutex::new(HashMap::new()),
        host_timer_callbacks: Mutex::new(HashMap::new()),
        transactions: Mutex::new(HashMap::new()),
        contexts: Mutex::new(HashMap::new()),
        phases: AtomicU64::new(0),
        source_generation_phases: AtomicU64::new(0),
        source_compaction_phases: AtomicU64::new(0),
        commits: AtomicU64::new(0),
        context_reads: AtomicU64::new(0),
        compaction_reads: AtomicU64::new(0),
        registry_reads: AtomicU64::new(0),
        agent_reads: AtomicU64::new(0),
        harness_opens: AtomicU64::new(0),
        hook_calls: AtomicU64::new(0),
        ranges: Mutex::new(HashMap::new()),
        context_refs: Mutex::new(context_bridge::ContextRefs::default()),
        turns: Mutex::default(),
        turned: tokio::sync::Notify::new(),
        prompt: prompt_bridge::PromptBridge::default(),
        registry: registry_bridge::RegistryBridge::default(),
        graph: graph_bridge::GraphBridge::default(),
        views: view_bridge::ViewBridge::default(),
        memory: memory_bridge::MemoryBridge::default(),
        models: model_bridge::ModelBridge::default(),
        documents: document_bridge::DocumentBridge::default(),
        submissions: submission_bridge::SubmissionBridge::default(),
        inspections: AtomicU64::new(0),
    })
}
/// Start the admission lane: requests that must keep wire order, replies and adoptions.
pub(crate) fn spawn_lane(
    hub: &Arc<Hub>,
    runtime: &tokio::runtime::Handle,
) -> (std::sync::mpsc::Sender<Work>, std::thread::JoinHandle<()>) {
    let (send, receive) = std::sync::mpsc::channel::<Work>();
    let admission = hub.clone();
    let handle = runtime.clone();
    let lane = std::thread::spawn(move || {
        let _entered = handle.enter();
        for work in receive {
            match work {
                Work::Request(value, context) => {
                    begin_request(admission.clone(), &handle, value, context)
                }
                Work::Reply(value) => admission.response(value),
                Work::Adopt(id, seq) => {
                    let adopt = admission.adoptions.lock().remove(&id);
                    if let Some(adopt) = adopt {
                        adopt(seq);
                    }
                }
                Work::Release(range) => {
                    admission.ranges.lock().remove(&range);
                }
            }
        }
    });
    (send, lane)
}
/// Route one frame from the host (a request, a callback reply, or a control frame).
pub(crate) fn receive(
    hub: &Arc<Hub>,
    runtime: &tokio::runtime::Handle,
    send: &std::sync::mpsc::Sender<Work>,
    value: JsonValue,
) {
    match value["type"].to_utf8().as_deref() {
        Some("host.ack") => {
            hub.host_ack(&value);
            return;
        }
        Some("codemode.vmEvent") => {
            hub.codemode_event(&value).unwrap();
            return;
        }
        Some("models.event") => {
            hub.model_event(&value).unwrap();
            return;
        }
        Some("callback.result") => {
            let id = field::<u64>(&value, "id").unwrap();
            let synchronous = matches!(hub.pending.lock().get(&id), Some(Pending::Sync(_)));
            if synchronous {
                hub.response(value);
            } else {
                send.send(Work::Reply(value)).unwrap();
            }
            return;
        }
        Some("context.release") => {
            send.send(Work::Release(field(&value, "range").unwrap()))
                .unwrap();
            return;
        }
        Some("storage.adopt") => {
            send.send(Work::Adopt(
                field(&value, "id").unwrap(),
                field(&value, "seq").unwrap(),
            ))
            .unwrap();
            return;
        }
        Some("abort") => {
            if let Some(controller) = hub
                .contexts
                .lock()
                .get(&field::<u64>(&value, "id").unwrap())
            {
                controller.abort_with_reason(SignalReason::Error(AbortError::new(
                    value["message"].to_utf8().unwrap(),
                )));
            }
            return;
        }
        _ => {}
    }
    let id = field::<u64>(&value, "id").unwrap();
    let context = hub.context(id);
    if let Some(message) = value["abortInitially"].to_utf8() {
        hub.contexts
            .lock()
            .get(&id)
            .unwrap()
            .abort_with_reason(SignalReason::Error(AbortError::new(message)));
    }
    if value["method"].to_utf8().as_deref() == Some("close")
        && value["admit"].as_bool() == Some(true)
        && value["reentrant"].as_bool() == Some(true)
    {
        let (hub, runtime) = (hub.clone(), runtime.clone());
        runtime
            .clone()
            .spawn_blocking(move || begin_request(hub, &runtime, value, context));
        return;
    }
    // Detaching a document watch reads the host clock synchronously.
    // Keep stdin available to receive that callback's reply.
    if matches!(
        value["method"].to_utf8().as_deref(),
        Some("graph.stop" | "graph.cancel" | "storage.adopt")
    ) {
        let (hub, runtime) = (hub.clone(), runtime.clone());
        runtime
            .clone()
            .spawn_blocking(move || begin_request(hub, &runtime, value, context));
        return;
    }
    // Registry operations are independent of the Session admission
    // line and may be called by a synchronous host callback on it.
    if value["method"].to_utf8().is_some_and(|method| {
        method.starts_with("registry.")
            || method == "runtimeRegistry"
            || method == "runtimeTimer"
            || method == "hostTimerFire"
            || method == "runtimeDisarmTimer"
            || method == "runtimeSettings"
            || method == "runtimeNow"
            || method == "runtimeReport"
            || (method.starts_with("graph.") && method != "graph.watch" && method != "graph.turn")
    }) {
        begin_request(hub.clone(), runtime, value, context);
    } else {
        send.send(Work::Request(value, context)).unwrap();
    }
}
/// The host is gone: fail every pending callback and refuse new ones.
pub(crate) fn close_input(hub: &Hub) {
    {
        let mut pending = hub.pending.lock();
        hub.input_closed.store(true, Ordering::Release);
        hub.turned.notify_waiters();
        for (_, waiter) in pending.drain() {
            let error = Err(Error::message("Test transport is closed"));
            match waiter {
                Pending::Sync(send) => {
                    let _ = send.send(error);
                }
                Pending::Async(send) => {
                    let _ = send.send(error);
                }
            }
        }
    }
}
pub(crate) enum Work {
    Request(JsonValue, Context),
    Reply(JsonValue),
    Adopt(u64, Seq),
    Release(u64),
}
fn begin_request(
    hub: Arc<Hub>,
    runtime: &tokio::runtime::Handle,
    value: JsonValue,
    context: Context,
) {
    let id = field::<u64>(&value, "id").unwrap();
    let admit = value["admit"].as_bool() == Some(true);
    let abort_receipt = (value["method"].to_utf8().as_deref() == Some("abortTask"))
        .then(|| {
            value["harness"]
                .as_u64()
                .zip(field::<TaskId>(&value, "task").ok())
        })
        .flatten();
    if let Some(task) = abort_receipt {
        hub.abort_receipts
            .lock()
            .entry(task)
            .or_default()
            .insert(id);
    }
    let owner = value["harness"]
        .as_u64()
        .and_then(|_| hub.session(&value).ok());
    // Begin Session operations in wire order before observing their futures.
    // Adoption and backend completion use the same lane, while synchronous
    // callback replies stay readable even if adoption calls the host clock.
    let boundary = value["method"].to_utf8().is_some_and(|method| {
        (method == "graph.start" || !method.starts_with("graph."))
            && method != "phase.turn"
            && method != "change.turn"
            && !method.starts_with("registry.")
            && method != "runtimeRegistry"
    });
    let operation = {
        let _entered = runtime.enter();
        hub.clone()
            .begin(value, context)
            .unwrap_or_else(|error| Box::pin(async { Err(error) }))
    };
    if admit {
        hub.emit(object([("type", encode(&"admitted")), ("id", encode(&id))]));
    }
    let respond = async move {
        let result = operation.await;
        if boundary {
            hub.graph.response_boundary().await;
        }
        hub.emit(match result {
            Ok(Some(value)) => object([("id", encode(&id)), ("value", value)]),
            Ok(None) => object([("id", encode(&id)), ("missing", encode(&true))]),
            Err(error) => object([("id", encode(&id)), ("error", error_value(&error))]),
        });
        hub.contexts.lock().remove(&id);
        if let Some(task) = abort_receipt {
            let mut receipts = hub.abort_receipts.lock();
            if let Some(pending) = receipts.get_mut(&task) {
                pending.remove(&id);
                if pending.is_empty() {
                    receipts.remove(&task);
                }
            }
        }
        hub.changed.notify_waiters();
    };
    if let Some(owner) = owner {
        owner.spawn(respond);
    } else {
        runtime.spawn(respond);
    }
}
