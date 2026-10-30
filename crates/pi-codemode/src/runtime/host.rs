//! The default sandbox: each execution gets its own thread and QuickJS VM.
//!
//! A fresh thread per run keeps termination simple: a runaway script cannot poison a later run.
//! Stopping sets an interrupt flag that the VM polls at every bridge call, loop iteration, promise
//! job and in QuickJS's interrupt handler, and does not wait for the thread: a long-running
//! built-in keeps its thread busy until it returns. Hosts that limit concurrency can track actual thread lifetime with
//! `on_worker_started` and `acquire_worker`.

use std::sync::Arc;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};

use super::execution::{CodemodeSandbox, CodemodeSandboxBaseOptions, CodemodeSandboxError, VmChannel, VmEvents, VmLauncher};
use super::protocol::{HostToWorkerMessage, VmStartData};
use super::vm::{DEFAULT_LOOP_ITERATION_LIMIT, DEFAULT_RECURSION_LIMIT, StartVmOptions, VmHandle, VmLimits, VmReport, spawn_vm};
use crate::types::{CodemodeError, CodemodeErrorKind, CodemodeTool};

/// Resolves when a VM thread has exited.
pub type WorkerExited = Shared<BoxFuture<'static, ()>>;

/// Releases a reservation made by `acquire_worker`; called once, when the thread exits or could not
/// be started.
pub type WorkerRelease = Box<dyn FnOnce() + Send>;

/// Reserves capacity right before a VM thread is created. Fail to deny admission without starting a
/// thread; the error message becomes the execution's `sandbox` error.
pub type AcquireWorker = Arc<dyn Fn() -> Result<WorkerRelease, String> + Send + Sync>;

/// Called once for each VM thread started by `execute()` or `start()`, with a future that resolves
/// when the thread actually exits, which may be after the result and `close()` settle. An error
/// fails the execution and stops the thread.
pub type OnWorkerStarted = Arc<dyn Fn(WorkerExited) -> Result<(), String> + Send + Sync>;

/// Options of [`CodemodeSandbox::new`].
#[derive(Clone, Default)]
pub struct CodemodeSandboxOptions {
    pub tools: Vec<CodemodeTool>,
    /// See [`CodemodeSandboxBaseOptions::globals`].
    pub globals: Vec<CodemodeTool>,
    /// See [`CodemodeSandboxBaseOptions::timeout_ms`].
    pub timeout_ms: Option<f64>,
    /// See [`CodemodeSandboxBaseOptions::memory_limit_bytes`].
    pub memory_limit_bytes: Option<u64>,
    /// Loop iterations and promise jobs an execution may run before it fails with
    /// `kind: "timeout"`, like the TS `interruptBudget`. Default: none; the deadline and abort
    /// signal stop computing scripts.
    pub interrupt_budget: Option<u64>,
    /// Loop iterations one function invocation may run in code the loop instrumentation does not
    /// reach (`eval`, `Function`). Default: [`DEFAULT_LOOP_ITERATION_LIMIT`]; `Some(u64::MAX)`
    /// disables it.
    pub loop_iteration_limit: Option<u64>,
    /// Maximum JS call depth. Default: [`DEFAULT_RECURSION_LIMIT`].
    pub recursion_limit: Option<usize>,
    /// See [`AcquireWorker`].
    pub acquire_worker: Option<AcquireWorker>,
    /// See [`OnWorkerStarted`].
    pub on_worker_started: Option<OnWorkerStarted>,
}

/// Starts each VM on its own thread.
pub struct ThreadLauncher {
    pub limits: VmLimits,
    pub acquire_worker: Option<AcquireWorker>,
    pub on_worker_started: Option<OnWorkerStarted>,
}

impl ThreadLauncher {
    pub fn new(limits: VmLimits) -> Self {
        ThreadLauncher { limits, acquire_worker: None, on_worker_started: None }
    }
}

/// Runs a release callback when dropped: on thread exit, or when the thread never started.
struct ReleaseGuard {
    release: Option<WorkerRelease>,
    exited: Option<futures::channel::oneshot::Sender<()>>,
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
        if let Some(exited) = self.exited.take() {
            let _ = exited.send(());
        }
    }
}

struct ThreadChannel {
    handle: VmHandle,
}

impl VmChannel for ThreadChannel {
    fn post(&self, message: HostToWorkerMessage) {
        self.handle.deliver(message);
    }

    fn stop(&self) -> BoxFuture<'static, ()> {
        self.handle.stop();
        Box::pin(async {})
    }
}

/// Spawn a VM thread whose reports go to `events`.
pub(crate) fn start_thread_vm(
    data: VmStartData,
    events: VmEvents,
    limits: VmLimits,
    release: Option<WorkerRelease>,
) -> Result<(VmHandle, WorkerExited), String> {
    let (exit_tx, exit_rx) = futures::channel::oneshot::channel();
    let exited: WorkerExited = exit_rx.map(|_| ()).boxed().shared();
    let guard = ReleaseGuard { release, exited: Some(exit_tx) };
    let post = Arc::new(move |report: VmReport| match report {
        VmReport::Message(message) => events.message(message),
        VmReport::Exhausted(message) => events.failure(CodemodeError::new(CodemodeErrorKind::Timeout, message)),
    });
    let handle = spawn_vm(StartVmOptions {
        data,
        limits,
        post,
        on_exit: Some(Box::new(move || drop(guard))),
    })
    .map_err(|error| format!("Failed to start worker: {error}"))?;
    Ok((handle, exited))
}

impl VmLauncher for ThreadLauncher {
    fn launch(&self, data: VmStartData, events: VmEvents) -> BoxFuture<'static, Result<Box<dyn VmChannel>, String>> {
        let result = (|| {
            let release = match &self.acquire_worker {
                Some(acquire) => Some(acquire()?),
                None => None,
            };
            let (handle, exited) = start_thread_vm(data, events, self.limits, release)?;
            if let Some(on_worker_started) = &self.on_worker_started
                && let Err(error) = on_worker_started(exited)
            {
                handle.stop();
                return Err(error);
            }
            Ok(Box::new(ThreadChannel { handle }) as Box<dyn VmChannel>)
        })();
        Box::pin(async move { result })
    }
}

impl CodemodeSandbox {
    /// Runs each execution's VM on its own thread, off the caller's threads.
    pub fn new(options: CodemodeSandboxOptions) -> Result<CodemodeSandbox, CodemodeSandboxError> {
        let launcher = ThreadLauncher {
            limits: VmLimits {
                interrupt_budget: options.interrupt_budget,
                loop_iteration_limit: Some(options.loop_iteration_limit.unwrap_or(DEFAULT_LOOP_ITERATION_LIMIT)),
                recursion_limit: options.recursion_limit.unwrap_or(DEFAULT_RECURSION_LIMIT),
            },
            acquire_worker: options.acquire_worker,
            on_worker_started: options.on_worker_started,
        };
        CodemodeSandbox::with_launcher(
            CodemodeSandboxBaseOptions {
                tools: options.tools,
                globals: options.globals,
                timeout_ms: options.timeout_ms,
                memory_limit_bytes: options.memory_limit_bytes,
            },
            Arc::new(launcher),
        )
    }
}
