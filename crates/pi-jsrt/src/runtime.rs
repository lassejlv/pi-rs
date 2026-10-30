//! The thread-safe runtime handle and its public API.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::ThreadId;
use std::time::Instant;

use boa_engine::object::FunctionObjectBuilder;
use boa_engine::{Context, JsResult, JsString, JsValue, NativeFunction};
use futures::channel::oneshot;
use parking_lot::{Mutex, RwLock};
use serde_json::Value;

use crate::convert;
use crate::engine::{self, Msg, Settled, state};
use crate::error::{self, Error, Result};

/// Sink for bytes written by JS to stdout / stderr (`console.*`, `process.stdout.write`).
pub type OutputSink = Arc<dyn Fn(&[u8]) + Send + Sync>;
/// Callback for uncaught exceptions and unhandled rejections: `(kind, error)`.
pub type UncaughtHandler = Arc<dyn Fn(&str, &Error) + Send + Sync>;
/// Callback for `process.exit(code)`.
pub type ExitHandler = Arc<dyn Fn(i32) + Send + Sync>;

/// A module provided by the embedder under a bare specifier.
#[derive(Debug, Clone)]
pub enum VirtualModule {
    /// ES module source text (TypeScript syntax is accepted and stripped).
    Esm(String),
    /// Plain JavaScript ES module source, parsed as is (no TypeScript stripping pass). Use it
    /// for prebuilt bundles and glue: it skips a full parse and print of the source.
    EsmJs(String),
    /// CommonJS source text (`module.exports = ...`).
    CommonJs(String),
    /// A JSON value exported as `default`.
    Json(Value),
    /// A JS expression evaluated once (as a script) on first import. Its value becomes
    /// the `default` export and its own enumerable properties become named exports.
    Expr(String),
    /// An existing JS object (e.g. built by the host with [`JsRuntime::with_context`] or
    /// returned from JS); exported like [`VirtualModule::Expr`].
    Object(JsHandle),
}

/// Configuration for [`JsRuntime::new`].
#[derive(Clone)]
pub struct RuntimeOptions {
    /// Working directory for `process.cwd()`, relative module resolution of
    /// top-level imports and relative fs paths. Defaults to the process cwd.
    pub cwd: Option<PathBuf>,
    /// Install Node.js compatibility (`process`, `Buffer`, `fetch`, `node:*` builtins).
    /// When `false` only ECMAScript + `console`, timers, `TextEncoder/Decoder`, `URL`,
    /// `AbortController`, `structuredClone`, `atob/btoa` and `queueMicrotask` exist
    /// (a sandbox suitable for code mode).
    pub node_compat: bool,
    /// `process.env` contents. `None` inherits the host environment.
    pub env: Option<HashMap<String, String>>,
    /// `process.argv`. Defaults to `["pi"]`.
    pub argv: Vec<String>,
    /// Tokio runtime used for async host functions and I/O. Defaults to the current
    /// runtime if called inside one, otherwise a private 2-worker runtime is created.
    pub tokio: Option<tokio::runtime::Handle>,
    /// Where JS stdout goes (default: the process stdout).
    pub stdout: Option<OutputSink>,
    /// Where JS stderr goes (default: the process stderr).
    pub stderr: Option<OutputSink>,
    /// Called for uncaught exceptions in timers/callbacks and unhandled rejections.
    /// Default: print the stack to the stderr sink.
    pub on_uncaught: Option<UncaughtHandler>,
    /// Called by `process.exit(code)`. Default: `process.exit` throws an error
    /// (the embedding process is never terminated by JS).
    pub on_exit: Option<ExitHandler>,
    /// Virtual modules available to `import` / `require` by exact specifier.
    pub virtual_modules: HashMap<String, VirtualModule>,
    /// Specifier aliases (exact or `prefix/` matches), e.g. `"@sinclair/typebox" -> "typebox"`.
    /// Targets may be other specifiers, virtual modules or absolute paths. A key that is an
    /// absolute file path redirects every specifier resolving to that file (e.g. relative imports
    /// of a source file replaced by a virtual module).
    pub aliases: HashMap<String, String>,
    /// Name of the JS thread.
    pub thread_name: String,
    /// Native stack size of the JS thread in bytes.
    pub thread_stack_size: usize,
    /// Maximum JS call recursion depth.
    pub recursion_limit: usize,
    /// Maximum VM stack size (values).
    pub stack_size_limit: usize,
    /// Optional loop iteration limit (per loop) to stop runaway scripts.
    pub loop_iteration_limit: Option<u64>,
    /// `process.execArgv`.
    pub exec_argv: Vec<String>,
    /// Read the process's standard input for `process.stdin` (Node mode). Off by default: an
    /// embedding application owns its stdin (pi's TUI, print and RPC modes read it), and
    /// `process.stdin` then never delivers data, like a stdin nobody can read from.
    pub stdin: bool,
    /// Give ES modules outside `node_modules` jiti-style `__filename`, `__dirname` and `require`
    /// bindings (default, how pi loads extensions). Node mode turns it off: Node's ES modules
    /// have none of them.
    pub esm_cjs_globals: bool,
    /// How `child_process.fork()` (and `cluster`) start another runtime as a Node-like process:
    /// the program and the arguments that precede the module path. `None` runs the current
    /// executable with [`crate::node::NODE_MARKER`], which an embedding binary hands to
    /// [`crate::node::main_if_requested`] first thing in `main`.
    pub node_exec: Option<Vec<String>>,
    /// Define a `gc()` global that forces a full collection (Node's `--expose-gc`).
    pub expose_gc: bool,
    /// Define the `__jsrtTimers` global that test runners use for fake timers: `freeze(nowMs)`
    /// installs a fake clock that timers created afterwards wait on, `advance(ms)` /
    /// `runNext(limitMs)` fire them, `next()`, `now()`, `count()`, `setNow(ms)`, `thaw()`, and
    /// `realTimeout(callback, ms)` schedules on the real clock while frozen.
    pub timer_control: bool,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        RuntimeOptions {
            cwd: None,
            node_compat: true,
            env: None,
            argv: vec!["pi".to_string()],
            tokio: None,
            stdout: None,
            stderr: None,
            on_uncaught: None,
            on_exit: None,
            virtual_modules: HashMap::new(),
            aliases: HashMap::new(),
            thread_name: "pi-jsrt".to_string(),
            thread_stack_size: 64 * 1024 * 1024,
            recursion_limit: 4096,
            stack_size_limit: 1024 * 1024,
            loop_iteration_limit: None,
            exec_argv: Vec::new(),
            esm_cjs_globals: true,
            stdin: false,
            node_exec: None,
            expose_gc: false,
            timer_control: false,
        }
    }
}

impl RuntimeOptions {
    /// Options for a sandbox without Node APIs (no fs, process, child_process, fetch).
    pub fn sandboxed() -> Self {
        RuntimeOptions {
            node_compat: false,
            ..Default::default()
        }
    }
}

/// State shared between the Rust side and the JS thread.
pub(crate) struct Shared {
    pub(crate) options: RuntimeOptions,
    ids: AtomicU64,
    refs: Mutex<HashMap<u64, u32>>,
    pub(crate) started: Instant,
    pub(crate) time_origin_ms: f64,
    pub(crate) on_uncaught: Option<UncaughtHandler>,
    pub(crate) virtual_modules: RwLock<HashMap<String, VirtualModule>>,
    pub(crate) aliases: RwLock<HashMap<String, String>>,
    /// Source text served instead of a file's contents ([`JsRuntime::set_file_source`]).
    pub(crate) file_sources: RwLock<HashMap<PathBuf, String>>,
    pub(crate) env: HashMap<String, String>,
    pub(crate) cwd: PathBuf,
}

impl Shared {
    pub(crate) fn next_id(&self) -> u64 {
        self.ids.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn adopt(&self, id: u64) {
        *self.refs.lock().entry(id).or_insert(0) += 1;
    }

    /// Returns true if the last Rust reference is gone.
    pub(crate) fn release(&self, id: u64) -> bool {
        let mut refs = self.refs.lock();
        match refs.get_mut(&id) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            _ => {
                refs.remove(&id);
                true
            }
        }
    }

    pub(crate) fn is_adopted(&self, id: u64) -> bool {
        self.refs.lock().contains_key(&id)
    }

    pub(crate) fn write_stdout(&self, data: &[u8]) {
        match &self.options.stdout {
            Some(s) => s(data),
            None => {
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(data);
                let _ = out.flush();
            }
        }
    }

    pub(crate) fn write_stderr(&self, data: &[u8]) {
        match &self.options.stderr {
            Some(s) => s(data),
            None => {
                use std::io::Write;
                let mut out = std::io::stderr().lock();
                let _ = out.write_all(data);
                let _ = out.flush();
            }
        }
    }
}

/// A blocking call from another thread, run on the JS thread while it waits in
/// [`JsRuntime::wait_servicing`].
type ServiceJob = Box<dyn FnOnce() + Send>;

pub(crate) struct Inner {
    tx: mpsc::Sender<Msg>,
    /// Set while the JS thread is blocked inside a host function waiting for another thread
    /// ([`JsRuntime::wait_servicing`]): blocking calls into JS from other threads are sent here
    /// and run re-entrantly instead of queueing behind the blocked host call (which deadlocks
    /// when the thread the JS thread waits for is the one calling into JS).
    servicing: Mutex<Option<mpsc::Sender<ServiceJob>>>,
    pub(crate) shared: Arc<Shared>,
    thread_id: ThreadId,
    tokio: tokio::runtime::Handle,
    owned_tokio: Mutex<Option<tokio::runtime::Runtime>>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if std::thread::current().id() != self.thread_id
            && let Some(j) = self.join.lock().take()
        {
            let _ = j.join();
        }
        if let Some(rt) = self.owned_tokio.lock().take() {
            rt.shutdown_background();
        }
    }
}

/// Handle to an embedded JavaScript runtime running on its own thread.
///
/// Cheap to clone; `Send + Sync`. The runtime stops when the last clone (and every
/// [`JsHandle`]) is dropped, or when [`JsRuntime::shutdown`] is called.
#[derive(Clone)]
pub struct JsRuntime {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for JsRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsRuntime").finish_non_exhaustive()
    }
}

type Reply<R> = oneshot::Sender<Result<R>>;

thread_local! {
    /// Context of the synchronous host function currently executing on this (JS) thread.
    static CURRENT_CTX: std::cell::Cell<*mut Context> = const { std::cell::Cell::new(std::ptr::null_mut()) };
}

/// Run `f` with the context of the synchronous host function that is currently executing on
/// this thread (re-entrant call into JS from Rust code invoked by JS). `None` when the current
/// thread is not inside a host function call.
pub(crate) fn with_current_ctx<R>(f: impl FnOnce(&mut Context) -> R) -> Option<R> {
    let ptr = CURRENT_CTX.with(|c| c.get());
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the pointer is set by `make_sync_host_fn` from the `&mut Context` it was called
    // with, and only while the host closure runs; the closure does not touch the context
    // itself, so this is a sequential re-borrow on the same thread.
    Some(f(unsafe { &mut *ptr }))
}

/// Whether the current thread is inside a synchronous host function call, where blocking calls
/// into the runtime are executed re-entrantly instead of failing with [`Error::Deadlock`].
pub fn in_host_call() -> bool {
    CURRENT_CTX.with(|c| !c.get().is_null())
}

/// Run `f` as a synchronous host function body: while it runs, blocking calls into this runtime
/// from this thread ([`JsHandle::call_blocking`], [`JsRuntime::with_context_blocking`],
/// [`crate::ctx::with_current`]) run re-entrantly on `ctx`. `f` must not use `ctx` itself.
pub(crate) fn enter_host_call<R>(ctx: &mut Context, f: impl FnOnce() -> R) -> R {
    let _guard = CurrentCtxGuard::enter(ctx);
    f()
}

struct CurrentCtxGuard(*mut Context);

impl CurrentCtxGuard {
    fn enter(ctx: &mut Context) -> Self {
        CurrentCtxGuard(CURRENT_CTX.with(|c| c.replace(ctx as *mut Context)))
    }
}

impl Drop for CurrentCtxGuard {
    fn drop(&mut self) {
        CURRENT_CTX.with(|c| c.set(self.0));
    }
}

/// Completion of a [`JsRuntime::wait_servicing`] wait; dropping it unresolved ends the wait.
struct Completion<R> {
    slot: Arc<Mutex<Option<Option<R>>>>,
    wake: Option<mpsc::Sender<ServiceJob>>,
}

impl<R: Send + 'static> Completion<R> {
    fn into_fn(self) -> impl FnOnce(R) + Send + 'static {
        move |r| {
            let mut this = self;
            *this.slot.lock() = Some(Some(r));
            if let Some(wake) = this.wake.take() {
                let _ = wake.send(Box::new(|| {}));
            }
        }
    }
}

impl<R> Drop for Completion<R> {
    fn drop(&mut self) {
        if let Some(wake) = self.wake.take() {
            let mut slot = self.slot.lock();
            if slot.is_none() {
                *slot = Some(None);
            }
            drop(slot);
            let _ = wake.send(Box::new(|| {}));
        }
    }
}

impl JsRuntime {
    /// Start a runtime with the given options.
    pub fn new(options: RuntimeOptions) -> Result<JsRuntime> {
        let (owned_tokio, tokio) = match options.tokio.clone().or_else(|| tokio::runtime::Handle::try_current().ok()) {
            Some(h) => (None, h),
            None => {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("pi-jsrt-io")
                    .enable_all()
                    .build()
                    .map_err(|e| Error::host(format!("failed to start tokio runtime: {e}")))?;
                let h = rt.handle().clone();
                (Some(rt), h)
            }
        };
        let cwd = match &options.cwd {
            Some(c) => c.clone(),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        };
        let env = options.env.clone().unwrap_or_else(|| std::env::vars().collect());
        let shared = Arc::new(Shared {
            on_uncaught: options.on_uncaught.clone(),
            virtual_modules: RwLock::new(options.virtual_modules.clone()),
            aliases: RwLock::new(options.aliases.clone()),
            file_sources: RwLock::default(),
            options,
            ids: AtomicU64::new(1),
            refs: Mutex::new(HashMap::new()),
            started: Instant::now(),
            time_origin_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64() * 1000.0)
                .unwrap_or(0.0),
            env,
            cwd: cwd.clone(),
        });
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let mut spawn_err = None;
        let inner = Arc::new_cyclic(|weak| {
            let init = engine::ThreadInit {
                rt: weak.clone(),
                shared: shared.clone(),
                tokio: tokio.clone(),
                tx: tx.clone(),
                rx,
                cwd,
            };
            let handle = std::thread::Builder::new()
                .name(shared.options.thread_name.clone())
                .stack_size(shared.options.thread_stack_size)
                .spawn(move || {
                    // Host functions run on this thread and may spawn tokio tasks.
                    let tokio_handle = init.tokio.clone();
                    let _guard = tokio_handle.enter();
                    engine::thread_main(init, ready_tx)
                });
            let (thread_id, join) = match handle {
                Ok(h) => (h.thread().id(), Some(h)),
                Err(e) => {
                    spawn_err = Some(e);
                    (std::thread::current().id(), None)
                }
            };
            Inner {
                tx,
                servicing: Mutex::new(None),
                shared: shared.clone(),
                thread_id,
                tokio,
                owned_tokio: Mutex::new(owned_tokio),
                join: Mutex::new(join),
            }
        });
        if let Some(e) = spawn_err {
            return Err(Error::host(format!("failed to spawn JS thread: {e}")));
        }
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(JsRuntime { inner }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(Error::Terminated),
        }
    }

    /// Start a runtime with default (Node-compatible) options.
    pub fn new_default() -> Result<JsRuntime> {
        Self::new(RuntimeOptions::default())
    }

    /// The tokio handle used by this runtime for async work.
    pub fn tokio(&self) -> &tokio::runtime::Handle {
        &self.inner.tokio
    }

    /// Whether the current thread is the JS thread.
    pub fn is_js_thread(&self) -> bool {
        std::thread::current().id() == self.inner.thread_id
    }

    /// Block the current thread until `start`'s completion callback is called (or dropped, which
    /// yields `None`). On the JS thread inside a synchronous host function, blocking calls into
    /// this runtime made by other threads meanwhile (e.g. by the thread doing the work) are run
    /// re-entrantly while waiting, so a host function may wait for a thread that calls back
    /// into JS without deadlocking.
    pub fn wait_servicing<R: Send + 'static>(&self, start: impl FnOnce(Box<dyn FnOnce(R) + Send>)) -> Option<R> {
        let slot: Arc<Mutex<Option<Option<R>>>> = Arc::new(Mutex::new(None));
        if !(self.is_js_thread() && in_host_call()) {
            let (tx, rx) = mpsc::sync_channel::<R>(1);
            start(Box::new(move |r| {
                let _ = tx.send(r);
            }));
            return around_blocking_wait(|| rx.recv().ok());
        }
        let (job_tx, job_rx) = mpsc::channel::<ServiceJob>();
        let previous = self.inner.servicing.lock().replace(job_tx.clone());
        start(Box::new(Completion { slot: slot.clone(), wake: Some(job_tx) }.into_fn()));
        let result = loop {
            if let Some(result) = slot.lock().take() {
                break result;
            }
            match job_rx.recv() {
                Ok(job) => job(),
                Err(_) => break None,
            }
        };
        *self.inner.servicing.lock() = previous;
        // Calls that raced with the end of the wait still run here; later ones are queued.
        while let Ok(job) = job_rx.try_recv() {
            job();
        }
        result
    }

    /// Stop the JS thread. Pending calls fail with [`Error::Terminated`].
    pub fn shutdown(&self) {
        let _ = self.inner.tx.send(Msg::Shutdown);
    }

    pub(crate) fn post(&self, f: impl FnOnce(&mut Context) + Send + 'static) -> Result<()> {
        self.inner.tx.send(Msg::Task(Box::new(f))).map_err(|_| Error::Terminated)
    }

    /// Queue `f` to run on the JS thread (promise jobs run after it) without waiting for it. Unlike
    /// [`JsRuntime::with_context`] it needs no async runtime, so threads that must not wait (or
    /// tokio workers that may all be busy) can hand work to JS.
    pub fn post_task(&self, f: impl FnOnce(&mut Context) + Send + 'static) -> Result<()> {
        self.post(f)
    }

    fn request<R: Send + 'static>(&self, f: impl FnOnce(&mut Context, Reply<R>) + Send + 'static) -> Result<oneshot::Receiver<Result<R>>> {
        let (tx, rx) = oneshot::channel();
        self.post(move |ctx| f(ctx, tx))?;
        Ok(rx)
    }

    async fn wait<R>(rx: Result<oneshot::Receiver<Result<R>>>) -> Result<R> {
        rx?.await.map_err(|_| Error::Terminated)?
    }

    fn wait_blocking<R>(&self, rx: Result<oneshot::Receiver<Result<R>>>) -> Result<R> {
        if self.is_js_thread() {
            return Err(Error::Deadlock);
        }
        let rx = rx?;
        around_blocking_wait(|| park_on(rx)).map_err(|_| Error::Terminated)?
    }

    /// Run `f` on the JS thread with direct access to the Boa [`Context`].
    ///
    /// This is the escape hatch for building host objects; see [`crate::ctx`] for helpers.
    pub async fn with_context<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Context) -> R + Send + 'static,
    {
        Self::wait(self.request(move |ctx, reply| {
            let _ = reply.send(Ok(f(ctx)));
        }))
        .await
    }

    /// Blocking variant of [`JsRuntime::with_context`] (must not be called on the JS thread).
    pub fn with_context_blocking<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Context) -> R + Send + 'static,
    {
        if self.is_js_thread() {
            return with_current_ctx(f).ok_or(Error::Deadlock);
        }
        let rx = self.request(move |ctx, reply| {
            let _ = reply.send(Ok(f(ctx)));
        });
        self.wait_blocking(rx)
    }

    /// Evaluate a classic script and return its completion value as JSON
    /// (awaiting it if it is a promise). Use `filename` ending in `.ts` to strip types.
    pub async fn eval_script(&self, code: impl Into<String>, filename: impl Into<String>) -> Result<Value> {
        let (code, filename) = (code.into(), filename.into());
        Self::wait(self.request(move |ctx, reply| {
            let r = crate::loader::eval_script(ctx, &code, &filename);
            reply_settled(ctx, r, reply, false);
        }))
        .await
        .map(settled_json)
    }

    /// Evaluate a script (see [`JsRuntime::eval_script`]) named `<eval>`.
    pub async fn eval(&self, code: impl Into<String>) -> Result<Value> {
        self.eval_script(code, "<eval>").await
    }

    /// Evaluate a script and keep the (awaited) result as a handle.
    pub async fn eval_handle(&self, code: impl Into<String>) -> Result<JsHandle> {
        let code = code.into();
        let id = Self::wait(self.request(move |ctx, reply| {
            let r = crate::loader::eval_script(ctx, &code, "<eval>");
            reply_settled(ctx, r, reply, true);
        }))
        .await?;
        Ok(self.settled_handle(id))
    }

    /// Evaluate ES module source. `filename` (relative to the cwd or absolute) names the
    /// module for relative imports and stack traces; `.ts` sources are transpiled.
    /// Returns the module namespace object.
    pub async fn eval_module(&self, source: impl Into<String>, filename: impl Into<String>) -> Result<JsHandle> {
        let (source, filename) = (source.into(), filename.into());
        let id = Self::wait(self.request(move |ctx, reply| match crate::loader::module_from_source(ctx, &source, &filename) {
            Ok(module) => crate::loader::evaluate_module(ctx, module, move |ctx, r| reply_settled_result(ctx, r, reply, true)),
            Err(e) => {
                let _ = reply.send(Err(e));
            }
        }))
        .await?;
        Ok(self.settled_handle(id))
    }

    /// `import(specifier)` resolved relative to the runtime cwd. Returns the namespace.
    pub async fn import(&self, specifier: impl Into<String>) -> Result<JsHandle> {
        let specifier = specifier.into();
        let id = Self::wait(self.request(move |ctx, reply| match crate::loader::module_for_specifier(ctx, &specifier, None) {
            Ok(module) => crate::loader::evaluate_module(ctx, module, move |ctx, r| reply_settled_result(ctx, r, reply, true)),
            Err(e) => {
                let _ = reply.send(Err(e));
            }
        }))
        .await?;
        Ok(self.settled_handle(id))
    }

    /// `require(specifier)` from the runtime cwd; returns `module.exports` as JSON.
    pub async fn require(&self, specifier: impl Into<String>) -> Result<JsHandle> {
        let specifier = specifier.into();
        let id = Self::wait(self.request(move |ctx, reply| {
            let r = crate::loader::require(ctx, &specifier, None);
            reply_settled(ctx, r, reply, true);
        }))
        .await?;
        Ok(self.settled_handle(id))
    }

    /// Register or replace a virtual module. Takes effect for modules not yet loaded.
    pub fn set_virtual_module(&self, specifier: impl Into<String>, module: VirtualModule) {
        self.inner.shared.virtual_modules.write().insert(specifier.into(), module);
    }

    /// Serve `source` as the contents of the file at `path` (absolute) when it is loaded, instead
    /// of reading it from disk. The module keeps the file's identity: relative imports,
    /// `import.meta.url` and the language chosen by its extension. Takes effect for files not yet
    /// loaded (test runners use it for source transforms such as vitest's mock hoisting).
    pub fn set_file_source(&self, path: impl Into<PathBuf>, source: impl Into<String>) {
        self.inner.shared.file_sources.write().insert(path.into(), source.into());
    }

    /// Register or replace a specifier alias.
    pub fn set_alias(&self, from: impl Into<String>, to: impl Into<String>) {
        self.inner.shared.aliases.write().insert(from.into(), to.into());
    }

    /// Forget all loaded file modules (ESM and CommonJS) so the next import re-reads them.
    /// Builtins and virtual modules stay cached.
    pub async fn clear_module_cache(&self) -> Result<()> {
        self.with_context(crate::loader::clear_file_cache).await
    }

    /// Register a synchronous host function at a dotted global path (e.g. `"pi.host.log"`).
    ///
    /// It runs on the JS thread; arguments are converted with [`crate::convert::to_json`]
    /// (functions become handle markers, adopt them with [`JsRuntime::handle`]).
    /// Returning `Err` throws in JS.
    pub async fn register_function<F>(&self, path: &str, f: F) -> Result<()>
    where
        F: Fn(&JsRuntime, Vec<Value>) -> Result<Value> + Send + Sync + 'static,
    {
        let path = path.to_string();
        let f: SyncHostFn = Arc::new(f);
        self.with_context(move |ctx| {
            let func = make_sync_host_fn(ctx, &path, f);
            set_global_path(ctx, &path, func.into())
        })
        .await?
    }

    /// Register an async host function at a dotted global path. Calling it from JS returns a
    /// promise that settles when the future completes (the future runs on tokio).
    pub async fn register_async_function<F, Fut>(&self, path: &str, f: F) -> Result<()>
    where
        F: Fn(JsRuntime, Vec<Value>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        let path = path.to_string();
        let f: AsyncHostFn = Arc::new(move |rt, args| Box::pin(f(rt, args)));
        self.with_context(move |ctx| {
            let func = make_async_host_fn(ctx, &path, f);
            set_global_path(ctx, &path, func.into())
        })
        .await?
    }

    /// Create an anonymous synchronous host function and return a marker value
    /// (`{"$hostFn": id}`) that turns into a JS function when passed to JS.
    pub async fn host_function<F>(&self, f: F) -> Result<Value>
    where
        F: Fn(&JsRuntime, Vec<Value>) -> Result<Value> + Send + Sync + 'static,
    {
        let f: SyncHostFn = Arc::new(f);
        self.with_context(move |ctx| {
            let func = make_sync_host_fn(ctx, "", f);
            let st = state(ctx);
            let id = st.next_id();
            st.host_fns.borrow_mut().insert(id, func.into());
            serde_json::json!({ convert::HOST_FN_KEY: id })
        })
        .await
    }

    /// Async variant of [`JsRuntime::host_function`].
    pub async fn host_async_function<F, Fut>(&self, f: F) -> Result<Value>
    where
        F: Fn(JsRuntime, Vec<Value>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        let f: AsyncHostFn = Arc::new(move |rt, args| Box::pin(f(rt, args)));
        self.with_context(move |ctx| {
            let func = make_async_host_fn(ctx, "", f);
            let st = state(ctx);
            let id = st.next_id();
            st.host_fns.borrow_mut().insert(id, func.into());
            serde_json::json!({ convert::HOST_FN_KEY: id })
        })
        .await
    }

    /// Adopt a handle marker (`{"$jsHandle": id}`) received from JS.
    ///
    /// Markers in host-function arguments that are not adopted before the host
    /// function returns (or its future completes) are released automatically.
    pub fn handle(&self, marker: &Value) -> Option<JsHandle> {
        let id = marker.get(convert::HANDLE_KEY)?.as_u64()?;
        self.inner.shared.adopt(id);
        Some(JsHandle {
            inner: Arc::new(HandleInner { id, rt: self.clone() }),
        })
    }

    fn settled_handle(&self, id: u64) -> JsHandle {
        // The JS side already counted one reference for us.
        JsHandle {
            inner: Arc::new(HandleInner { id, rt: self.clone() }),
        }
    }

    /// Number of entries in the JS-side handle table (diagnostics).
    pub async fn handle_count(&self) -> Result<usize> {
        self.with_context(|ctx| crate::ctx::handle_count(ctx)).await
    }

    /// Wait until the event loop has no referenced work left: no referenced timers, pending
    /// async operations or referenced handles (sockets, servers, child processes, workers,
    /// message ports with listeners...). This is when Node would emit `beforeExit` and exit.
    pub async fn wait_idle(&self) -> Result<()> {
        let rx = self.request(|ctx, reply: Reply<oneshot::Receiver<()>>| {
            let (tx, rx) = oneshot::channel();
            state(ctx).add_idle_waiter(tx);
            let _ = reply.send(Ok(rx));
        });
        let idle = Self::wait(rx).await?;
        idle.await.map_err(|_| Error::Terminated)
    }

    /// Call `internal[name](arg)` (a function of the glue's private `internal` object) and await
    /// the result as JSON.
    pub(crate) async fn call_internal(&self, name: &'static str, arg: Value) -> Result<Value> {
        Self::wait(self.request(move |ctx, reply| {
            let internal = engine::internal(ctx);
            let r = internal.get(JsString::from(name), ctx).and_then(|f| {
                let f = f.as_callable().ok_or_else(|| engine::type_error(format!("internal.{name} is not a function")))?;
                let arg = convert::from_json(&arg, ctx)?;
                f.call(&JsValue::undefined(), &[arg], ctx)
            });
            reply_settled(ctx, r, reply, false);
        }))
        .await
        .map(settled_json)
    }

    /// Number of active timers (diagnostics).
    pub async fn timer_count(&self) -> Result<usize> {
        self.with_context(|ctx| engine::timer_count(ctx)).await
    }
}

/// Block the current thread on a future, parking between polls.
/// Wraps every wait of a non-JS thread for the JS thread (see [`set_blocking_wait_hook`]).
pub type BlockingWaitHook = fn(&mut dyn FnMut());

static BLOCKING_WAIT_HOOK: std::sync::OnceLock<BlockingWaitHook> = std::sync::OnceLock::new();

/// Install a wrapper around every blocking wait of another thread for the JS thread (blocking
/// calls into JS, `wait_servicing` off the JS thread). An embedder uses it to release locks the
/// waiting thread holds that the JS thread may need meanwhile (pi's TUI releases its UI lock: a
/// frame rendering a JS component waits for the JS thread, which may wait for a UI update that
/// needs that lock). The first installed hook wins.
pub fn set_blocking_wait_hook(hook: BlockingWaitHook) {
    let _ = BLOCKING_WAIT_HOOK.set(hook);
}

fn around_blocking_wait<R>(f: impl FnOnce() -> R) -> R {
    let Some(hook) = BLOCKING_WAIT_HOOK.get() else { return f() };
    let mut f = Some(f);
    let mut out = None;
    hook(&mut || {
        if let Some(f) = f.take() {
            out = Some(f());
        }
    });
    match out {
        Some(r) => r,
        // A hook that did not call the wait: run it unwrapped.
        None => (f.take().expect("wait ran or not"))(),
    }
}

fn park_on<F: Future>(fut: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        if let std::task::Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

type SyncHostFn = Arc<dyn Fn(&JsRuntime, Vec<Value>) -> Result<Value> + Send + Sync>;
type AsyncHostFn = Arc<dyn Fn(JsRuntime, Vec<Value>) -> futures::future::BoxFuture<'static, Result<Value>> + Send + Sync>;

fn runtime_of(ctx: &Context) -> Option<JsRuntime> {
    state(ctx).rt.upgrade().map(|inner| JsRuntime { inner })
}

fn convert_args(ctx: &mut Context, args: &[JsValue]) -> JsResult<Vec<Value>> {
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        out.push(convert::to_json(a, ctx).map_err(|e| error::to_boa(&e, ctx))?);
    }
    Ok(out)
}

fn make_sync_host_fn(ctx: &mut Context, name: &str, f: SyncHostFn) -> boa_engine::object::builtins::JsFunction {
    // SAFETY: captures only an Arc of a Send+Sync Rust closure (no GC values).
    let native = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let rt = runtime_of(ctx).ok_or_else(|| engine::type_error("runtime is shutting down"))?;
            let (converted, created) = engine::collect_created(ctx, |ctx| convert_args(ctx, args));
            let result = converted.and_then(|args| {
                let r = {
                    let _guard = CurrentCtxGuard::enter(ctx);
                    f(&rt, args)
                };
                match r {
                    Ok(v) => convert::from_json(&v, ctx),
                    Err(e) => Err(error::to_boa(&e, ctx)),
                }
            });
            state(ctx).sweep_unadopted(&created);
            result
        })
    };
    let realm = ctx.realm().clone();
    FunctionObjectBuilder::new(&realm, native)
        .name(JsString::from(name.rsplit('.').next().unwrap_or(name)))
        .build()
}

fn make_async_host_fn(ctx: &mut Context, name: &str, f: AsyncHostFn) -> boa_engine::object::builtins::JsFunction {
    // SAFETY: captures only an Arc of a Send+Sync Rust closure (no GC values).
    let native = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let rt = runtime_of(ctx).ok_or_else(|| engine::type_error("runtime is shutting down"))?;
            let (converted, created) = engine::collect_created(ctx, |ctx| convert_args(ctx, args));
            let args = match converted {
                Ok(a) => a,
                Err(e) => {
                    state(ctx).sweep_unadopted(&created);
                    let v = e.into_opaque(ctx).unwrap_or_default();
                    return Ok(boa_engine::object::builtins::JsPromise::reject(boa_engine::JsError::from_opaque(v), ctx)?.into());
                }
            };
            let fut = f(rt, args);
            Ok(engine::spawn_promise(ctx, async move {
                let r = fut.await;
                Box::new(move |ctx: &mut Context| {
                    state(ctx).sweep_unadopted(&created);
                    match r {
                        Ok(v) => convert::from_json(&v, ctx),
                        Err(e) => Err(error::to_boa(&e, ctx)),
                    }
                }) as engine::Finish
            }))
        })
    };
    let realm = ctx.realm().clone();
    FunctionObjectBuilder::new(&realm, native)
        .name(JsString::from(name.rsplit('.').next().unwrap_or(name)))
        .build()
}

/// Assign `value` at a dotted path below `globalThis`, creating intermediate objects.
pub(crate) fn set_global_path(ctx: &mut Context, path: &str, value: JsValue) -> Result<()> {
    let mut obj = ctx.global_object();
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    let Some((last, parents)) = parts.split_last() else {
        return Err(Error::host("empty global path"));
    };
    for p in parents {
        let key = JsString::from(*p);
        let next = obj.get(key.clone(), ctx).map_err(|e| error::from_boa(e, ctx))?;
        obj = match next.as_object() {
            Some(o) => o,
            None => {
                let o = boa_engine::JsObject::with_object_proto(ctx.intrinsics());
                obj.set(key, o.clone(), true, ctx).map_err(|e| error::from_boa(e, ctx))?;
                o
            }
        };
    }
    obj.set(JsString::from(*last), value, true, ctx).map_err(|e| error::from_boa(e, ctx))?;
    Ok(())
}

fn settled_json(s: Settled) -> Value {
    match s {
        Settled::Json(v) => v,
        Settled::Handle(_) => Value::Null,
    }
}

/// Await `value` and reply with JSON or a new owned handle.
pub(crate) fn reply_settled<R: From<Settled> + Send + 'static>(ctx: &mut Context, value: JsResult<JsValue>, reply: Reply<R>, as_handle: bool) {
    engine::when_settled(ctx, value, move |ctx, r| reply_settled_result(ctx, r, reply, as_handle));
}

pub(crate) fn reply_settled_result<R: From<Settled> + Send + 'static>(
    ctx: &mut Context,
    r: std::result::Result<JsValue, JsValue>,
    reply: Reply<R>,
    as_handle: bool,
) {
    let out = match r {
        Ok(v) if as_handle => Ok(Settled::Handle(state(ctx).new_owned_handle(v))),
        Ok(v) => convert::to_json(&v, ctx).map(Settled::Json),
        Err(e) => Err(error::from_thrown(&e, ctx)),
    };
    let _ = reply.send(out.map(R::from));
}

impl From<Settled> for u64 {
    fn from(s: Settled) -> u64 {
        match s {
            Settled::Handle(id) => id,
            Settled::Json(_) => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// JsHandle
// ---------------------------------------------------------------------------

struct HandleInner {
    id: u64,
    rt: JsRuntime,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        let _ = self.rt.inner.tx.send(Msg::Release(self.id));
    }
}

/// A reference to a JS value (function, object, module namespace...) kept alive on the
/// JS thread. Cheap to clone; `Send + Sync`. Released when the last clone is dropped.
///
/// [`JsCallback`] and [`JsObjectHandle`] are aliases used for readability.
#[derive(Clone)]
pub struct JsHandle {
    inner: Arc<HandleInner>,
}

/// A JS function stored for later invocation from Rust.
pub type JsCallback = JsHandle;
/// A JS object stored for later method calls from Rust.
pub type JsObjectHandle = JsHandle;

impl std::fmt::Debug for JsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JsHandle({})", self.inner.id)
    }
}

#[derive(Clone)]
enum Target {
    Call,
    Method(String),
    Construct,
    Get(String),
}

impl JsHandle {
    /// Handle id (stable for the handle's lifetime).
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    /// The runtime owning this handle.
    pub fn runtime(&self) -> &JsRuntime {
        &self.inner.rt
    }

    /// A marker value that becomes this JS value again when passed to JS
    /// (as an argument or in a host function result).
    pub fn to_value(&self) -> Value {
        serde_json::json!({ convert::HANDLE_KEY: self.inner.id })
    }

    fn invoke(&self, target: Target, args: Vec<Value>, as_handle: bool) -> Result<oneshot::Receiver<Result<Settled>>> {
        let id = self.inner.id;
        self.inner.rt.request(move |ctx, reply| {
            let value = invoke_on_thread(ctx, id, target, args);
            match value {
                Ok(v) => reply_settled(ctx, Ok(v), reply, as_handle),
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            }
        })
    }

    async fn run(&self, target: Target, args: Vec<Value>) -> Result<Value> {
        JsRuntime::wait(self.invoke(target, args, false)).await.map(settled_json)
    }

    /// Re-entrant invocation from Rust code running inside a host call on the JS thread.
    fn run_reentrant(&self, target: Target, args: Vec<Value>, as_handle: bool) -> Result<Settled> {
        let id = self.inner.id;
        with_current_ctx(|ctx| {
            let value = invoke_on_thread(ctx, id, target, args)?;
            let value = match value.as_promise() {
                Some(p) => {
                    engine::run_microtasks(ctx);
                    match p.state() {
                        boa_engine::builtins::promise::PromiseState::Fulfilled(v) => v,
                        boa_engine::builtins::promise::PromiseState::Rejected(e) => {
                            return Err(error::from_thrown(&e, ctx));
                        }
                        boa_engine::builtins::promise::PromiseState::Pending => {
                            return Err(Error::host(
                                "re-entrant call into the JS runtime returned a pending promise",
                            ));
                        }
                    }
                }
                None => value,
            };
            if as_handle {
                Ok(Settled::Handle(state(ctx).new_owned_handle(value)))
            } else {
                convert::to_json(&value, ctx).map(Settled::Json)
            }
        })
        .unwrap_or(Err(Error::Deadlock))
    }

    fn run_blocking(&self, target: Target, args: Vec<Value>) -> Result<Value> {
        if self.inner.rt.is_js_thread() {
            return self.run_reentrant(target, args, false).map(settled_json);
        }
        self.run_blocking_settled(target, args, false).map(settled_json)
    }

    async fn run_handle(&self, target: Target, args: Vec<Value>) -> Result<JsHandle> {
        let id = JsRuntime::wait(self.invoke(target, args, true)).await?;
        Ok(self.inner.rt.settled_handle(u64::from(id)))
    }

    fn run_handle_blocking(&self, target: Target, args: Vec<Value>) -> Result<JsHandle> {
        if self.inner.rt.is_js_thread() {
            let id = u64::from(self.run_reentrant(target, args, true)?);
            return Ok(self.inner.rt.settled_handle(id));
        }
        let id = self.run_blocking_settled(target, args, true)?;
        Ok(self.inner.rt.settled_handle(u64::from(id)))
    }

    /// A blocking call into JS from another thread. The call is queued for the JS thread; if the
    /// JS thread meanwhile blocks in [`JsRuntime::wait_servicing`] (a host function waiting on
    /// another thread) before the queued call ran, the call is handed to that servicing loop
    /// instead. Checking for servicing only once, before queueing, raced: a JS handler queued a
    /// UI call that calls back into JS (e.g. `addAutocompleteProvider` reading the provider's
    /// trigger characters), then blocked on a UI getter (`ctx.ui.theme`) before the UI thread's
    /// call reached the queue, so each thread waited for the other forever.
    fn run_blocking_settled(&self, target: Target, args: Vec<Value>, as_handle: bool) -> Result<Settled> {
        let rt = &self.inner.rt;
        // Whichever path claims the call first runs it.
        let claimed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = std::thread::current();
        let (serviced_tx, serviced_rx) = mpsc::sync_channel::<Result<Settled>>(1);
        let id = self.inner.id;
        let queued = {
            let claimed = claimed.clone();
            let (t, a) = (target.clone(), args.clone());
            rt.request(move |ctx, reply| {
                if claimed.swap(true, Ordering::SeqCst) {
                    return;
                }
                let value = invoke_on_thread(ctx, id, t, a);
                match value {
                    Ok(v) => reply_settled(ctx, Ok(v), reply, as_handle),
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            })?
        };
        let mut queued = std::pin::pin!(queued);
        struct Unpark(std::thread::Thread);
        impl std::task::Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = std::task::Waker::from(Arc::new(Unpark(waiter.clone())));
        let mut cx = std::task::Context::from_waker(&waker);
        let mut handed_over = false;
        around_blocking_wait(move || loop {
            if let std::task::Poll::Ready(r) = queued.as_mut().poll(&mut cx) {
                match r {
                    Ok(r) => return r,
                    // The queued request was dropped unrun: it was claimed by the servicing path.
                    Err(_) if handed_over => {}
                    Err(_) => return Err(Error::Terminated),
                }
            }
            if let Ok(r) = serviced_rx.try_recv() {
                return r;
            }
            if !handed_over && !claimed.load(Ordering::SeqCst) {
                let sender = rt.inner.servicing.lock().clone();
                if let Some(sender) = sender {
                    let this = self.clone();
                    let claimed = claimed.clone();
                    let serviced_tx = serviced_tx.clone();
                    let waiter = waiter.clone();
                    let (t, a) = (target.clone(), args.clone());
                    let job: ServiceJob = Box::new(move || {
                        if claimed.swap(true, Ordering::SeqCst) {
                            return;
                        }
                        let _ = serviced_tx.send(this.run_reentrant(t, a, as_handle));
                        waiter.unpark();
                    });
                    handed_over = sender.send(job).is_ok();
                }
            }
            std::thread::park_timeout(std::time::Duration::from_millis(2));
        })
    }

    /// Call the function with JSON arguments; awaits a returned promise; returns JSON.
    pub async fn call(&self, args: Vec<Value>) -> Result<Value> {
        self.run(Target::Call, args).await
    }

    /// Alias of [`JsHandle::call`].
    pub async fn call_async(&self, args: Vec<Value>) -> Result<Value> {
        self.call(args).await
    }

    /// Blocking [`JsHandle::call`] for non-async threads (e.g. a render thread).
    /// On the JS thread this only works re-entrantly from inside a synchronous host function
    /// (the call runs immediately); otherwise it fails with [`Error::Deadlock`].
    pub fn call_blocking(&self, args: Vec<Value>) -> Result<Value> {
        self.run_blocking(Target::Call, args)
    }

    /// Call `this[name](...args)`.
    pub async fn call_method(&self, name: &str, args: Vec<Value>) -> Result<Value> {
        self.run(Target::Method(name.to_string()), args).await
    }

    /// Blocking [`JsHandle::call_method`].
    pub fn call_method_blocking(&self, name: &str, args: Vec<Value>) -> Result<Value> {
        self.run_blocking(Target::Method(name.to_string()), args)
    }

    /// Call the function and keep the (awaited) result as a handle.
    pub async fn call_handle(&self, args: Vec<Value>) -> Result<JsHandle> {
        self.run_handle(Target::Call, args).await
    }

    /// Blocking [`JsHandle::call_handle`].
    pub fn call_handle_blocking(&self, args: Vec<Value>) -> Result<JsHandle> {
        self.run_handle_blocking(Target::Call, args)
    }

    /// Call a method and keep the (awaited) result as a handle.
    pub async fn call_method_handle(&self, name: &str, args: Vec<Value>) -> Result<JsHandle> {
        self.run_handle(Target::Method(name.to_string()), args).await
    }

    /// `new this(...args)`, returned as a handle.
    pub async fn construct(&self, args: Vec<Value>) -> Result<JsHandle> {
        self.run_handle(Target::Construct, args).await
    }

    /// Read a property as JSON (does not await promises... except it does: a promise
    /// property value is awaited like any other result).
    pub async fn get(&self, name: &str) -> Result<Value> {
        self.run(Target::Get(name.to_string()), vec![]).await
    }

    /// Blocking [`JsHandle::get`].
    pub fn get_blocking(&self, name: &str) -> Result<Value> {
        self.run_blocking(Target::Get(name.to_string()), vec![])
    }

    /// Read a property as a handle.
    pub async fn get_handle(&self, name: &str) -> Result<JsHandle> {
        self.run_handle(Target::Get(name.to_string()), vec![]).await
    }

    /// Blocking [`JsHandle::get_handle`].
    pub fn get_handle_blocking(&self, name: &str) -> Result<JsHandle> {
        self.run_handle_blocking(Target::Get(name.to_string()), vec![])
    }

    /// Set a property from JSON.
    pub async fn set(&self, name: &str, value: Value) -> Result<()> {
        let id = self.inner.id;
        let name = name.to_string();
        JsRuntime::wait(self.inner.rt.request(move |ctx, reply| {
            let r = (|| {
                let obj = handle_value(ctx, id)?
                    .as_object()
                    .ok_or_else(|| Error::host("handle is not an object"))?;
                let v = convert::from_json(&value, ctx).map_err(|e| error::from_boa(e, ctx))?;
                obj.set(JsString::from(name.as_str()), v, true, ctx).map_err(|e| error::from_boa(e, ctx))?;
                Ok(())
            })();
            let _ = reply.send(r);
        }))
        .await
    }

    /// Convert the value itself to JSON.
    pub async fn to_json(&self) -> Result<Value> {
        let id = self.inner.id;
        JsRuntime::wait(self.inner.rt.request(move |ctx, reply| {
            let r = handle_value(ctx, id).and_then(|v| convert::to_json(&v, ctx));
            let _ = reply.send(r);
        }))
        .await
    }

    /// `typeof value`.
    pub async fn type_of(&self) -> Result<String> {
        let id = self.inner.id;
        JsRuntime::wait(self.inner.rt.request(move |ctx, reply| {
            let _ = reply.send(handle_value(ctx, id).map(|v| v.type_of().to_string()));
        }))
        .await
    }
}

fn handle_value(ctx: &mut Context, id: u64) -> Result<JsValue> {
    state(ctx).get_handle(id).ok_or(Error::InvalidHandle(id))
}

fn invoke_on_thread(ctx: &mut Context, id: u64, target: Target, args: Vec<Value>) -> Result<JsValue> {
    let value = handle_value(ctx, id)?;
    let mut js_args = Vec::with_capacity(args.len());
    for a in &args {
        js_args.push(convert::from_json(a, ctx).map_err(|e| error::from_boa(e, ctx))?);
    }
    let r = match target {
        Target::Call => match value.as_callable() {
            Some(f) => f.call(&JsValue::undefined(), &js_args, ctx),
            None => return Err(Error::js("TypeError", "handle is not a function")),
        },
        Target::Method(name) => {
            let obj = value.to_object(ctx).map_err(|e| error::from_boa(e, ctx))?;
            let f = obj.get(JsString::from(name.as_str()), ctx).map_err(|e| error::from_boa(e, ctx))?;
            match f.as_callable() {
                Some(f) => f.call(&value, &js_args, ctx),
                None => return Err(Error::js("TypeError", format!("{name} is not a function"))),
            }
        }
        Target::Construct => match value.as_constructor() {
            Some(c) => c.construct(&js_args, None, ctx).map(JsValue::from),
            None => return Err(Error::js("TypeError", "handle is not a constructor")),
        },
        Target::Get(name) => {
            let obj = value.to_object(ctx).map_err(|e| error::from_boa(e, ctx))?;
            obj.get(JsString::from(name.as_str()), ctx)
        }
    };
    r.map_err(|e| error::from_boa(e, ctx))
}
