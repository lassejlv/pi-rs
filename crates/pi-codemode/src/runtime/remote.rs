//! Runs the VM somewhere else, reached over an RPC boundary (another process, a container, a
//! remote service), while tools keep running in the caller.
//!
//! The caller keeps the host role ([`CodemodeSandbox::remote`]: tools, call records, deadline,
//! abort) and the remote side only runs the VM ([`serve_codemode_remote`]).
//!
//! Protocol, one RPC call per execution:
//!
//! ```text
//! client                                     remote
//! remote.execute(request, exchange) ───────▶ starts the VM (on its own thread)
//!                                  ◀─────── exchange([...outputs, call])   one call per tool call
//! runs the tool, replies with result ──────▶ VM continues
//!                                  ◀─────── exchange([...outputs, done])   terminal, reply ignored
//! execute() resolves ◀────────────────────── VM stopped
//! ```
//!
//! `exchange` is a function the client passes in; over an RPC transport it becomes a stub that
//! calls back into the client. Output is buffered on the remote side and sent with the next call or
//! the terminal message, so a script that prints in a loop does not make one RPC per line. Each
//! `exchange` carries at most one call, so concurrent tool calls are concurrent RPCs. Exchanges are
//! started in order, which keeps output ordered before the call or result that follows.
//!
//! Termination: when the client finishes first (deadline, abort, `close()`), it answers every
//! pending and later `exchange` with `Stop`, and the remote stops the VM at that point. A script
//! that computes without calling a tool runs until its interrupt budget is used up. The remote
//! reports that as a `failure` message, which the client turns into the same `kind: "timeout"`
//! error a local VM produces.
//!
//! Failure: if the RPC call fails or returns without a terminal message, the execution fails with
//! `kind: "sandbox"`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use super::execution::{CodemodeSandbox, CodemodeSandboxBaseOptions, CodemodeSandboxError, VmChannel, VmEvent, VmEvents, VmLauncher};
use super::host::start_thread_vm;
use super::protocol::{HostToWorkerMessage, VmStartData, WorkerToHostMessage};
use super::vm::{DEFAULT_LOOP_ITERATION_LIMIT, DEFAULT_RECURSION_LIMIT, VmHandle, VmLimits};

/// Default interrupt budget of [`serve_codemode_remote`]: loop iterations and promise jobs one
/// execution may run. A remote VM keeps computing after the client's deadline until it calls a
/// tool, so the budget is what bounds it.
pub const DEFAULT_INTERRUPT_BUDGET: u64 = 10_000_000;
use crate::types::{CodemodeError, CodemodeErrorKind, CodemodeOutputItem, CodemodeTool};

/// What the client asks the remote to run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeRemoteRequest {
    pub data: VmStartData,
    /// Requested interrupt budget. The remote may lower it, see [`CodemodeServeOptions`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt_budget: Option<u64>,
}

/// `{ type: "failure", error }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodemodeRemoteFailure {
    #[serde(rename = "type")]
    pub kind: CodemodeRemoteFailureTag,
    pub error: CodemodeError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeRemoteFailureTag {
    Failure,
}

/// What the remote sends to the client: VM messages, or a failure outside the VM bridge.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CodemodeRemoteMessage {
    Failure(CodemodeRemoteFailure),
    Vm(WorkerToHostMessage),
}

impl CodemodeRemoteMessage {
    pub fn failure(error: CodemodeError) -> Self {
        CodemodeRemoteMessage::Failure(CodemodeRemoteFailure { kind: CodemodeRemoteFailureTag::Failure, error })
    }
}

/// `{ type: "stop" }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodeRemoteStop {
    #[serde(rename = "type")]
    pub kind: CodemodeRemoteStopTag,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeRemoteStopTag {
    Stop,
}

/// The client's answer to an exchange: the result of the call the exchange carried, `None` (JSON
/// `null`) when it carried no call, or `Stop` once the client no longer wants the execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CodemodeRemoteReply {
    Stop(CodemodeRemoteStop),
    Result(HostToWorkerMessage),
    None,
}

impl CodemodeRemoteReply {
    pub fn stop() -> Self {
        CodemodeRemoteReply::Stop(CodemodeRemoteStop { kind: CodemodeRemoteStopTag::Stop })
    }
}

/// Sends messages to the client and returns its reply. An `Err` is a transport failure.
pub type CodemodeRemoteExchange =
    Arc<dyn Fn(Vec<CodemodeRemoteMessage>) -> BoxFuture<'static, Result<CodemodeRemoteReply, String>> + Send + Sync>;

/// The remote end. Anything with this method works, for example an RPC stub or an in-process
/// [`CodemodeRemoteServer`].
pub trait CodemodeRemote: Send + Sync {
    /// Run one execution, calling back through `exchange`. Resolves when the script has settled or
    /// was stopped.
    fn execute(&self, request: CodemodeRemoteRequest, exchange: CodemodeRemoteExchange) -> BoxFuture<'static, Result<(), String>>;
}

/// Picks the remote for one execution; called once per `execute()`.
pub type CodemodeRemoteFactory = Arc<dyn Fn() -> Result<Arc<dyn CodemodeRemote>, String> + Send + Sync>;

/// Options of [`CodemodeSandbox::remote`].
#[derive(Clone)]
pub struct RemoteCodemodeSandboxOptions {
    pub tools: Vec<CodemodeTool>,
    pub globals: Vec<CodemodeTool>,
    pub timeout_ms: Option<f64>,
    pub memory_limit_bytes: Option<u64>,
    /// Returns the remote that runs one execution, called once per `execute()`.
    pub remote: CodemodeRemoteFactory,
    /// Interrupt budget requested from the remote. Default: the remote's default.
    pub interrupt_budget: Option<u64>,
}

impl RemoteCodemodeSandboxOptions {
    pub fn new(remote: impl Fn() -> Result<Arc<dyn CodemodeRemote>, String> + Send + Sync + 'static) -> Self {
        RemoteCodemodeSandboxOptions {
            tools: Vec::new(),
            globals: Vec::new(),
            timeout_ms: None,
            memory_limit_bytes: None,
            remote: Arc::new(remote),
            interrupt_budget: None,
        }
    }
}

/// Output buffered on the remote before it is flushed without waiting for a call.
const FLUSH_ITEMS: usize = 1_000;
const FLUSH_CHARS: usize = 1 << 20;

/// Limits of [`serve_codemode_remote`].
#[derive(Clone, Debug, Default)]
pub struct CodemodeServeOptions {
    /// Upper bound for a request's interrupt budget, and the budget when a request names none.
    /// Default: [`DEFAULT_INTERRUPT_BUDGET`].
    pub max_interrupt_budget: Option<u64>,
    /// Loop iterations one function invocation may run in code the loop instrumentation does not
    /// reach. Default: [`DEFAULT_LOOP_ITERATION_LIMIT`].
    pub loop_iteration_limit: Option<u64>,
    /// Upper bound for a request's VM memory, and the limit when a request names none.
    pub max_memory_limit_bytes: Option<u64>,
    /// Maximum JS call depth. Default: [`DEFAULT_RECURSION_LIMIT`].
    pub recursion_limit: Option<usize>,
    /// The timezone the VMs' `Date` uses (IANA name or POSIX `TZ` string), the serving host's
    /// own; `None`: this process's `TZ`.
    pub timezone: Option<String>,
}

/// Start `future` and poll it once right away, so its synchronous part runs now (in order with
/// the caller's other calls); spawn the rest.
fn start_now<T: Send + 'static>(mut future: BoxFuture<'static, T>, then: impl FnOnce(T) + Send + 'static) {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut cx) {
        then(value);
        return;
    }
    tokio::spawn(async move { then(future.await) });
}

struct ServerState {
    handle: Option<VmHandle>,
    ended: bool,
    buffered: Vec<CodemodeRemoteMessage>,
    buffered_chars: usize,
}

struct Server {
    state: Mutex<ServerState>,
    exchange: CodemodeRemoteExchange,
    /// The batch with the terminal message to deliver, set once by `end`.
    terminal: Mutex<Option<Vec<CodemodeRemoteMessage>>>,
    ended: tokio_util::sync::CancellationToken,
}

impl Server {
    fn take_buffer(state: &mut ServerState) -> Vec<CodemodeRemoteMessage> {
        state.buffered_chars = 0;
        std::mem::take(&mut state.buffered)
    }

    fn end(&self, terminal: Option<CodemodeRemoteMessage>) {
        let mut state = self.state.lock();
        if state.ended {
            return;
        }
        state.ended = true;
        let mut batch = Self::take_buffer(&mut state);
        drop(state);
        if let Some(terminal) = terminal {
            batch.push(terminal);
            *self.terminal.lock() = Some(batch);
        }
        self.ended.cancel();
    }

    fn reply(self: &Arc<Self>, reply: Result<CodemodeRemoteReply, String>) {
        let reply = match reply {
            Ok(reply) => reply,
            Err(_) => return self.end(None),
        };
        let state = self.state.lock();
        if state.ended {
            return;
        }
        match reply {
            CodemodeRemoteReply::None => {}
            CodemodeRemoteReply::Stop(_) => {
                drop(state);
                self.end(None);
            }
            CodemodeRemoteReply::Result(message) => {
                if let Some(handle) = &state.handle {
                    handle.deliver(message);
                }
            }
        }
    }

    fn send(self: &Arc<Self>, batch: Vec<CodemodeRemoteMessage>) {
        let server = self.clone();
        start_now((self.exchange)(batch), move |reply| server.reply(reply));
    }

    fn on_message(self: &Arc<Self>, message: WorkerToHostMessage) {
        let mut state = self.state.lock();
        if state.ended {
            return;
        }
        match message {
            WorkerToHostMessage::Output { item } => {
                state.buffered_chars += match &item {
                    // Characters as JavaScript counts them (UTF-16 code units).
                    CodemodeOutputItem::Text { text, .. } => crate::js::utf16_len(text),
                    CodemodeOutputItem::Image { data, .. } => crate::js::utf16_len(data),
                };
                state.buffered.push(CodemodeRemoteMessage::Vm(WorkerToHostMessage::Output { item }));
                if state.buffered.len() >= FLUSH_ITEMS || state.buffered_chars >= FLUSH_CHARS {
                    let batch = Self::take_buffer(&mut state);
                    drop(state);
                    self.send(batch);
                }
            }
            message @ WorkerToHostMessage::OutputUtf16 { .. } => {
                if let WorkerToHostMessage::OutputUtf16 { data, .. } = &message {
                    state.buffered_chars += data.len();
                }
                state.buffered.push(CodemodeRemoteMessage::Vm(message));
                if state.buffered.len() >= FLUSH_ITEMS || state.buffered_chars >= FLUSH_CHARS {
                    let batch = Self::take_buffer(&mut state);
                    drop(state);
                    self.send(batch);
                }
            }
            message @ WorkerToHostMessage::Started { .. } => state.buffered.push(CodemodeRemoteMessage::Vm(message)),
            // Sent at once: a host replaying a script waits for it to see that the script issued no
            // further call.
            message @ (WorkerToHostMessage::Call { .. } | WorkerToHostMessage::CallUtf16 { .. } | WorkerToHostMessage::Idle { .. }) => {
                let mut batch = Self::take_buffer(&mut state);
                batch.push(CodemodeRemoteMessage::Vm(message));
                drop(state);
                self.send(batch);
            }
            terminal => {
                drop(state);
                self.end(Some(CodemodeRemoteMessage::Vm(terminal)));
            }
        }
    }
}

/// Remote side of [`CodemodeSandbox::remote`]: runs one execution's VM and forwards everything it
/// reports through `exchange`. Resolves once the VM has stopped and the terminal message has been
/// delivered, or the client asked to stop. Never fails.
pub async fn serve_codemode_remote(request: CodemodeRemoteRequest, exchange: CodemodeRemoteExchange, options: CodemodeServeOptions) {
    let max_budget = options.max_interrupt_budget.unwrap_or(DEFAULT_INTERRUPT_BUDGET);
    let budget = request.interrupt_budget.unwrap_or(max_budget).min(max_budget);
    let limits = VmLimits {
        interrupt_budget: Some(budget),
        loop_iteration_limit: Some(options.loop_iteration_limit.unwrap_or(DEFAULT_LOOP_ITERATION_LIMIT)),
        recursion_limit: options.recursion_limit.unwrap_or(DEFAULT_RECURSION_LIMIT),
    };
    let memory_limit_bytes = match (request.data.memory_limit_bytes, options.max_memory_limit_bytes) {
        (None, max) => max,
        (Some(requested), None) => Some(requested),
        (Some(requested), Some(max)) => Some(requested.min(max)),
    };
    let data = VmStartData { memory_limit_bytes, timezone: options.timezone.clone(), ..request.data };

    let server = Arc::new(Server {
        state: Mutex::new(ServerState { handle: None, ended: false, buffered: Vec::new(), buffered_chars: 0 }),
        exchange,
        terminal: Mutex::new(None),
        ended: tokio_util::sync::CancellationToken::new(),
    });
    let (tx, mut rx) = mpsc::unbounded_channel();
    let events = VmEvents::from_sender(tx);
    match start_thread_vm(data, events, limits, None) {
        Ok((handle, _exited)) => server.state.lock().handle = Some(handle),
        Err(message) => server.end(Some(CodemodeRemoteMessage::failure(CodemodeError::new(CodemodeErrorKind::Sandbox, message)))),
    }
    loop {
        tokio::select! {
            biased;
            _ = server.ended.cancelled() => break,
            event = rx.recv() => match event {
                Some(VmEvent::Message(message)) => server.on_message(message),
                Some(VmEvent::Raw(value)) => match super::protocol::parse_worker_to_host_message(value) {
                    Some(message) => server.on_message(message),
                    None => server.end(Some(CodemodeRemoteMessage::failure(CodemodeError::new(CodemodeErrorKind::Sandbox, "unknown message from the VM")))),
                },
                Some(VmEvent::Failure(error)) => server.end(Some(CodemodeRemoteMessage::failure(error))),
                None => server.end(None),
            },
        }
    }
    let terminal = server.terminal.lock().take();
    if let Some(batch) = terminal {
        let _ = (server.exchange)(batch).await;
    }
    let handle = server.state.lock().handle.take();
    if let Some(handle) = handle {
        handle.stop();
    }
}

/// An in-process [`CodemodeRemote`] that serves executions with [`serve_codemode_remote`]: the
/// remote end of a [`CodemodeSandbox::remote`] sandbox, for example behind an RPC server.
#[derive(Clone, Debug, Default)]
pub struct CodemodeRemoteServer {
    pub options: CodemodeServeOptions,
}

impl CodemodeRemote for CodemodeRemoteServer {
    fn execute(&self, request: CodemodeRemoteRequest, exchange: CodemodeRemoteExchange) -> BoxFuture<'static, Result<(), String>> {
        let options = self.options.clone();
        Box::pin(async move {
            serve_codemode_remote(request, exchange, options).await;
            Ok(())
        })
    }
}

struct ClientState {
    stopped: AtomicBool,
    settled: AtomicBool,
    waiting: Mutex<HashMap<u64, oneshot::Sender<CodemodeRemoteReply>>>,
}

struct RemoteChannel {
    state: Arc<ClientState>,
}

impl VmChannel for RemoteChannel {
    fn post(&self, message: HostToWorkerMessage) {
        let waiter = self.state.waiting.lock().remove(&message.id());
        if let Some(waiter) = waiter {
            let _ = waiter.send(CodemodeRemoteReply::Result(message));
        }
    }

    fn stop(&self) -> BoxFuture<'static, ()> {
        self.state.stopped.store(true, Ordering::SeqCst);
        let waiting: Vec<_> = self.state.waiting.lock().drain().map(|(_, waiter)| waiter).collect();
        for waiter in waiting {
            let _ = waiter.send(CodemodeRemoteReply::stop());
        }
        Box::pin(async {})
    }
}

/// Launches each VM on a remote.
pub struct RemoteLauncher {
    pub remote: CodemodeRemoteFactory,
    pub interrupt_budget: Option<u64>,
}

impl VmLauncher for RemoteLauncher {
    fn launch(&self, data: VmStartData, events: VmEvents) -> BoxFuture<'static, Result<Box<dyn VmChannel>, String>> {
        let state =
            Arc::new(ClientState { stopped: AtomicBool::new(false), settled: AtomicBool::new(false), waiting: Mutex::new(HashMap::new()) });
        let exchange_state = state.clone();
        let exchange_events = events.clone();
        let exchange: CodemodeRemoteExchange = Arc::new(move |messages: Vec<CodemodeRemoteMessage>| {
            let state = &exchange_state;
            if state.stopped.load(Ordering::SeqCst) {
                return Box::pin(async { Ok(CodemodeRemoteReply::stop()) });
            }
            // Register the waiter first: the host may answer a call synchronously, for example when
            // the tool fails before returning a future.
            let call = messages.iter().find_map(|message| match message {
                CodemodeRemoteMessage::Vm(WorkerToHostMessage::Call { id, .. } | WorkerToHostMessage::CallUtf16 { id, .. }) => Some(*id),
                _ => None,
            });
            let reply: BoxFuture<'static, Result<CodemodeRemoteReply, String>> = match call {
                Some(id) => {
                    let (tx, rx) = oneshot::channel();
                    state.waiting.lock().insert(id, tx);
                    Box::pin(async move { Ok(rx.await.unwrap_or_else(|_| CodemodeRemoteReply::stop())) })
                }
                None => Box::pin(async { Ok(CodemodeRemoteReply::None) }),
            };
            for message in messages {
                match message {
                    CodemodeRemoteMessage::Failure(failure) => {
                        state.settled.store(true, Ordering::SeqCst);
                        exchange_events.failure(failure.error);
                    }
                    CodemodeRemoteMessage::Vm(message) => {
                        if message.is_terminal() {
                            state.settled.store(true, Ordering::SeqCst);
                        }
                        exchange_events.message(message);
                    }
                }
            }
            reply
        });

        let target = match (self.remote)() {
            Ok(target) => target,
            Err(error) => {
                let message = format!("Failed to reach the remote sandbox: {error}");
                return Box::pin(async move { Err(message) });
            }
        };
        // The remote's VM keeps the remote's timezone.
        let request = CodemodeRemoteRequest { data: VmStartData { timezone: None, ..data }, interrupt_budget: self.interrupt_budget };
        let run_state = state.clone();
        tokio::spawn(async move {
            let result = target.execute(request, exchange).await;
            match result {
                Ok(()) => {
                    if !run_state.settled.load(Ordering::SeqCst) && !run_state.stopped.load(Ordering::SeqCst) {
                        events.failure(CodemodeError::new(CodemodeErrorKind::Sandbox, "Remote sandbox returned before the script settled"));
                    }
                }
                Err(error) => {
                    if !run_state.stopped.load(Ordering::SeqCst) {
                        events.failure(CodemodeError::new(CodemodeErrorKind::Sandbox, format!("Remote sandbox failed: {error}")));
                    }
                }
            }
        });
        Box::pin(async move { Ok(Box::new(RemoteChannel { state }) as Box<dyn VmChannel>) })
    }
}

impl CodemodeSandbox {
    /// Runs each execution's VM on a remote while tools run here. Same API and results as
    /// [`CodemodeSandbox::new`]. Differences: after a deadline or abort the remote VM keeps
    /// computing until it next calls a tool or uses up its interrupt budget.
    pub fn remote(options: RemoteCodemodeSandboxOptions) -> Result<CodemodeSandbox, CodemodeSandboxError> {
        CodemodeSandbox::with_launcher(
            CodemodeSandboxBaseOptions {
                tools: options.tools,
                globals: options.globals,
                timeout_ms: options.timeout_ms,
                memory_limit_bytes: options.memory_limit_bytes,
            },
            Arc::new(RemoteLauncher { remote: options.remote, interrupt_budget: options.interrupt_budget }),
        )
    }
}

/// The TS name of the remote sandbox; construct it with [`CodemodeSandbox::remote`].
pub type RemoteCodemodeSandbox = CodemodeSandbox;
