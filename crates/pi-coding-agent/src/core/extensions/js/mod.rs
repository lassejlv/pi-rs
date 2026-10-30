//! JavaScript extension bridge: loads `.ts` / `.js` extensions in the embedded `pi-jsrt` runtime.
//!
//! One [`JsExtensionHost`] (one JS runtime) serves all JavaScript extensions loaded for the same
//! working directory and extension cache generation; a reload (`clearExtensionCache()`) starts a
//! new one. Extensions import pi's packages as virtual modules implemented in `js/*.js` (in the
//! crate root) on top of host functions:
//!
//! - `__piHost.call(op, payload)`: synchronous, runs on the JS thread;
//! - `__piHost.callAsync(op, payload)`: returns a promise, runs on tokio.
//!
//! JS objects reach Rust as JSON (functions become handle markers, see `pi_jsrt::convert`); the
//! bridge turns them into the Rust extension API types: handlers become [`ExtensionHandler`]s
//! returning [`ExtensionEventResult::Json`], tools become [`ToolDefinition`]s whose closures call
//! into JS, components become [`JsComponent`]s rendering through blocking calls.

mod api;
mod builtin;
mod component;
mod context;
mod messages;
mod native;
mod provider;
mod sdk;
mod tui_helpers;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Weak};

use futures::future::BoxFuture;
use parking_lot::Mutex;
use pi_jsrt::{JsHandle, JsRuntime, RuntimeOptions, VirtualModule};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::core::extensions::runner::ExtensionErrorWithStack;
use crate::core::extensions::types::*;

pub use component::{JsComponent, JsEditorComponent};

const BRIDGE_JS: &str = include_str!("../../../../js/bridge.js");
/// The real `typebox` 1.3.27 (`scripts/vendor-typebox.mjs`): entry points and shared chunks.
const TYPEBOX_MODULES: &[(&str, &str)] = &[
    ("typebox", include_str!("../../../../js/vendor/typebox/index.js")),
    ("typebox/value", include_str!("../../../../js/vendor/typebox/value.js")),
    ("typebox/compile", include_str!("../../../../js/vendor/typebox/compile.js")),
    ("pi-rs:typebox/chunk-1", include_str!("../../../../js/vendor/typebox/chunk-1.js")),
    ("pi-rs:typebox/chunk-2", include_str!("../../../../js/vendor/typebox/chunk-2.js")),
];
/// The real `marked` 18.0.11 (`scripts/vendor-marked.mjs`), re-exported by pi-tui as `Marked`.
const MARKED_JS: &str = include_str!("../../../../js/vendor/marked.js");
const PI_TUI_JS: &str = include_str!("../../../../js/pi-tui.js");
const PI_AI_JS: &str = include_str!("../../../../js/pi-ai.js");
const PI_AI_CORE_JS: &str = include_str!("../../../../js/pi-ai/core.js");
const PI_AI_FAUX_JS: &str = include_str!("../../../../js/pi-ai/faux.js");
const PI_AI_RUST_JS: &str = include_str!("../../../../js/pi-ai/rust.js");
const PI_AI_MODELS_JS: &str = include_str!("../../../../js/pi-ai-models.js");
const PI_AI_PROVIDERS_ALL_JS: &str = include_str!("../../../../js/pi-ai-providers-all.js");
const PI_AGENT_CORE_JS: &str = include_str!("../../../../js/pi-agent-core.js");
const PI_CODING_AGENT_JS: &str = include_str!("../../../../js/pi-coding-agent.js");
const PI_SDK_JS: &str = include_str!("../../../../js/pi-sdk.js");

/// Specifier of the internal bridge module.
const BRIDGE_SPECIFIER: &str = "pi-rs:bridge";

/// Convert a JS error into an error that keeps the JS stack (reported by the runner).
pub(crate) fn js_error(err: pi_jsrt::Error) -> anyhow::Error {
    match err {
        pi_jsrt::Error::Js(e) => {
            anyhow::Error::new(ExtensionErrorWithStack { message: e.message.clone(), stack: e.stack.clone() })
        }
        other => anyhow::Error::new(ExtensionErrorWithStack { message: other.to_string(), stack: None }),
    }
}

/// A Rust callback callable from JS (`rustCallback(id)` in bridge.js).
pub(crate) type RustCallback = Arc<dyn Fn(Vec<Value>) -> anyhow::Result<Value> + Send + Sync>;

/// An async Rust callback callable from JS (`rustAsyncCallback(id)` in bridge.js).
pub(crate) type AsyncRustCallback = Arc<dyn Fn(Vec<Value>) -> BoxFuture<'static, anyhow::Result<Value>> + Send + Sync>;

/// A context registered for JS (`makeContext(id, kind)` in bridge.js).
#[derive(Clone)]
pub(crate) enum CtxEntry {
    Base(ExtensionContext),
    Tool(ExtensionToolContext),
    Command(ExtensionCommandContext),
    Replaced(ReplacedSessionContext),
    Trust(ProjectTrustContext),
}

impl CtxEntry {
    pub(crate) fn base(&self) -> Option<&ExtensionContext> {
        match self {
            Self::Base(c) => Some(c),
            Self::Tool(c) => Some(&c.ctx),
            Self::Command(c) => Some(&c.ctx),
            Self::Replaced(c) => Some(&c.cmd.ctx),
            Self::Trust(_) => None,
        }
    }
}

/// Id allocator plus map, the shape of every bridge registry.
pub(crate) struct Registry<T> {
    items: Mutex<HashMap<u64, T>>,
}

impl<T: Clone> Registry<T> {
    fn new() -> Self {
        Self { items: Mutex::new(HashMap::new()) }
    }

    pub(crate) fn insert(&self, value: T) -> u64 {
        let id = next_id();
        self.items.lock().insert(id, value);
        id
    }

    pub(crate) fn insert_with_id(&self, id: u64, value: T) {
        self.items.lock().insert(id, value);
    }

    pub(crate) fn get(&self, id: u64) -> Option<T> {
        self.items.lock().get(&id).cloned()
    }

    pub(crate) fn remove(&self, id: u64) -> Option<T> {
        self.items.lock().remove(&id)
    }

    /// The most recently registered entry.
    pub(crate) fn latest(&self) -> Option<T> {
        let items = self.items.lock();
        items.keys().max().and_then(|id| items.get(id).cloned())
    }
}

/// The original JavaScript values behind Rust objects the bridge built from them (provider config
/// functions, provider objects), keyed by the Rust object's address, so handing the Rust object
/// back to JS (`ctx.modelRegistry.getRegisteredProviderConfig()`, `getProvider()`, ...) yields
/// the very value the extension registered, as TS does. Entries hold a `Weak` of the Rust
/// object, which keeps its address from being reused while the entry exists.
#[derive(Default)]
pub(crate) struct Origins {
    items: Mutex<HashMap<usize, Origin>>,
}

struct Origin {
    alive: Box<dyn Fn() -> bool + Send + Sync>,
    handle: JsHandle,
}

impl Origins {
    fn key<T: ?Sized>(object: &Arc<T>) -> usize {
        Arc::as_ptr(object) as *const () as usize
    }

    /// Record that `object` was built from the JS value `handle`.
    pub(crate) fn record<T: ?Sized + Send + Sync + 'static>(&self, object: &Arc<T>, handle: JsHandle) {
        let weak = Arc::downgrade(object);
        let mut items = self.items.lock();
        items.retain(|_, origin| (origin.alive)());
        items.insert(Self::key(object), Origin { alive: Box::new(move || weak.strong_count() > 0), handle });
    }

    /// The JS value `object` was built from (a handle marker), if this bridge built it.
    pub(crate) fn get<T: ?Sized>(&self, object: &Arc<T>) -> Option<Value> {
        let items = self.items.lock();
        let origin = items.get(&Self::key(object))?;
        (origin.alive)().then(|| origin.handle.to_value())
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Shared state of one JS extension host. Host functions hold it weakly so the JS runtime can
/// stop once every extension using it is gone.
pub(crate) struct Bridge {
    pub(crate) rt: JsRuntime,
    /// The `pi-rs:bridge` module namespace.
    pub(crate) glue: JsHandle,
    pub(crate) cwd: String,
    pub(crate) apis: Registry<ExtensionAPIRef>,
    pub(crate) unsubscribes: Registry<Unsubscribe>,
    pub(crate) contexts: Registry<CtxEntry>,
    /// Context ids by host pointer, so repeated events share one JS ctx object.
    pub(crate) context_ids: Mutex<HashMap<usize, u64>>,
    pub(crate) tokens: Registry<CancellationToken>,
    pub(crate) callbacks: Registry<RustCallback>,
    pub(crate) async_callbacks: Registry<AsyncRustCallback>,
    pub(crate) callback_ids: Mutex<HashMap<usize, u64>>,
    /// Session managers handed to JavaScript (`ctx.sessionManager`, `newSession({ setup })`,
    /// `SessionManager.create()`, ...), see `context::session_manager_id`.
    pub(crate) session_managers: Registry<context::SessionManagerRef>,
    /// Ids of host-owned session managers by address (one JS object per session manager).
    pub(crate) session_manager_ids: Mutex<HashMap<usize, u64>>,
    pub(crate) streams: Registry<Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Value>>>>,
    pub(crate) components: component::ComponentRegistries,
    pub(crate) builtins: Registry<builtin::BuiltinTool>,
    /// Rust pi-ai objects behind JavaScript proxies (providers, API implementations, auth).
    pub(crate) natives: Registry<native::Native>,
    /// Native ids of Rust providers handed to JS by address (one JS object per provider).
    pub(crate) native_provider_ids: Mutex<HashMap<usize, u64>>,
    /// The JavaScript values behind Rust objects built from them (see [`Origins`]).
    pub(crate) origins: Origins,
    /// Settings managers created from JavaScript (`SettingsManager.create()`, ...).
    pub(crate) settings_managers: Registry<Arc<crate::core::settings_manager::SettingsManager>>,
    /// Model runtimes handed to JavaScript (`ModelRuntime.create()`, `ctx.modelRegistry.runtime`).
    pub(crate) model_runtimes: Registry<Arc<crate::core::model_runtime::ModelRuntime>>,
    pub(crate) model_runtime_ids: Mutex<HashMap<usize, u64>>,
    /// Model registries created from JavaScript (`new ModelRegistry(runtime)`).
    pub(crate) model_registries: Registry<Arc<crate::core::model_registry::ModelRegistry>>,
    /// Ids of host-owned settings managers by address (`session.settingsManager`).
    pub(crate) settings_manager_ids: Mutex<HashMap<usize, u64>>,
    /// Resource loaders created from JavaScript (`new DefaultResourceLoader()`).
    pub(crate) resource_loaders: Registry<Arc<crate::core::resource_loader::DefaultResourceLoader>>,
    /// Agent sessions created from JavaScript (`createAgentSession()`).
    pub(crate) agent_sessions: Registry<Arc<crate::core::agent_session::AgentSession>>,
    /// Rust tool-call hooks handed to JavaScript and hook calls in progress (`session.agent`).
    pub(crate) tool_hooks: Registry<sdk::ToolHook>,
    pub(crate) hook_contexts: Registry<sdk::HookContext>,
    /// Loaded extensions handed to JavaScript (held weakly) and their ids by address.
    pub(crate) extension_refs: Registry<std::sync::Weak<parking_lot::RwLock<Extension>>>,
    pub(crate) extension_ids: Mutex<HashMap<usize, u64>>,
    /// Extension runtimes handed to JavaScript (`createExtensionRuntime()`).
    pub(crate) ext_runtimes: Registry<Arc<ExtensionRuntime>>,
    pub(crate) ext_runtime_ids: Mutex<HashMap<usize, u64>>,
    /// Package managers created from JavaScript (`new DefaultPackageManager()`).
    pub(crate) package_managers: Registry<Arc<crate::core::package_manager::DefaultPackageManager>>,
    /// Unregisters the JavaScript session-resource cleanup (`registerSessionResourceCleanup`).
    session_cleanup: Mutex<Option<Box<dyn FnOnce() + Send + Sync>>>,
    /// Ordered executor for UI calls made from JS (see [`Bridge::ui_dispatch`]).
    ui_tx: Mutex<std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>>,
}

impl Bridge {
    /// Register a context for JS and return its id. Contexts sharing a host share an id.
    pub(crate) fn register_ctx(&self, entry: CtxEntry) -> u64 {
        if let CtxEntry::Base(ctx) = &entry {
            let key = Arc::as_ptr(ctx.host()) as *const () as usize;
            if let Some(id) = self.context_ids.lock().get(&key) {
                return *id;
            }
            let id = self.contexts.insert(entry);
            self.context_ids.lock().insert(key, id);
            return id;
        }
        self.contexts.insert(entry)
    }

    /// The call that received context `id` ended. TS contexts stay usable until the runner goes
    /// stale (extensions keep them for later UI updates), so JS keeps the ctx object working and
    /// frees the Rust entry once the extension no longer references it (`detachContext`).
    pub(crate) fn detach_ctx(&self, id: u64, kind: &str) {
        let glue = self.glue.clone();
        let kind = kind.to_string();
        tokio_spawn(async move {
            let _ = glue.call_method("detachContext", vec![json!(id), json!(kind)]).await;
        });
    }

    /// Register a Rust callback for JS and return its id.
    pub(crate) fn register_callback(&self, callback: RustCallback) -> u64 {
        self.callbacks.insert(callback)
    }

    /// Register a callback keyed by the identity of `key` (stable ids for recurring closures).
    pub(crate) fn register_callback_keyed(&self, key: usize, make: impl FnOnce() -> RustCallback) -> u64 {
        let mut ids = self.callback_ids.lock();
        if let Some(id) = ids.get(&key) {
            return *id;
        }
        let id = self.callbacks.insert(make());
        ids.insert(key, id);
        id
    }

    /// Mirror a Rust cancellation token as a JS AbortSignal for the lifetime of the guard.
    pub(crate) fn signal_for(self: &Arc<Self>, token: &CancellationToken) -> (Value, SignalGuard) {
        let id = next_id();
        let done = CancellationToken::new();
        let value = json!({ "id": id, "aborted": token.is_cancelled() });
        if !token.is_cancelled() {
            let token = token.clone();
            let done2 = done.clone();
            let glue = self.glue.clone();
            tokio_spawn(async move {
                tokio::select! {
                    _ = token.cancelled() => {
                        let _ = glue.call_method("abortSignal", vec![json!(id)]).await;
                    }
                    _ = done2.cancelled() => {
                        let _ = glue.call_method("releaseSignal", vec![json!(id)]).await;
                    }
                }
            });
        }
        (value, SignalGuard(done))
    }

    /// A signal that follows `token` until the token is cancelled or the bridge is dropped.
    pub(crate) fn detached_signal_for(self: &Arc<Self>, token: &CancellationToken) -> Value {
        let (value, guard) = self.signal_for(token);
        std::mem::forget(guard);
        value
    }

    /// Run a UI call made from JS on the ordered UI executor. JS never blocks on the UI (or on
    /// locks held by a thread that waits for JS to render).
    pub(crate) fn ui_dispatch(&self, f: impl FnOnce() + Send + 'static) {
        let _ = self.ui_tx.lock().send(Box::new(f));
    }

    /// Run a UI call on the ordered executor and wait for its result (getters).
    pub(crate) fn ui_dispatch_wait<R: Send + 'static>(&self, f: impl FnOnce() -> R + Send + 'static) -> anyhow::Result<R> {
        // On the JS thread, keep servicing blocking calls into JS while waiting: the UI call may
        // call back into JS (e.g. `showOverlay` focuses a JS component, which sets its `focused`).
        self.rt
            .wait_servicing(|done| {
                self.ui_dispatch(move || done(f()));
            })
            .ok_or_else(|| anyhow::anyhow!("UI executor stopped"))
    }

    /// Resolve a token id sent by JS (`tokenFor(signal)`).
    pub(crate) fn token(&self, value: &Value) -> Option<CancellationToken> {
        value.as_u64().and_then(|id| self.tokens.get(id))
    }

    /// Adopt a JS function or object handle marker.
    pub(crate) fn handle(&self, value: &Value) -> Option<JsHandle> {
        self.rt.handle(value)
    }

    /// Call an exported function of the bridge module.
    pub(crate) async fn glue_call(&self, name: &str, args: Vec<Value>) -> anyhow::Result<Value> {
        self.glue.call_method(name, args).await.map_err(js_error)
    }

    /// Blocking variant of [`Self::glue_call`] for synchronous Rust callers (renderers).
    pub(crate) fn glue_call_blocking(&self, name: &str, args: Vec<Value>) -> anyhow::Result<Value> {
        self.glue.call_method_blocking(name, args).map_err(js_error)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        if let Some(unregister) = self.session_cleanup.get_mut().take() {
            unregister();
        }
    }
}

/// Stops mirroring a token into JS when dropped.
pub(crate) struct SignalGuard(CancellationToken);

impl Drop for SignalGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(crate) fn tokio_spawn(fut: impl std::future::Future<Output = ()> + Send + 'static) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            crate::utils::uncaught::spawn_on(&handle, fut);
        }
        Err(_) => {
            crate::utils::uncaught::spawn_thread(move || futures::executor::block_on(fut));
        }
    }
}

/// Tokio runtime for the async host functions of extension runtimes. A shared host outlives the
/// tokio runtime that happened to create it (a CLI `block_on`, a test), so it gets its own.
fn extension_io_runtime() -> anyhow::Result<tokio::runtime::Handle> {
    static RUNTIME: std::sync::OnceLock<std::io::Result<tokio::runtime::Runtime>> = std::sync::OnceLock::new();
    let rt = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("pi-ext-io").enable_all().build()
    });
    match rt {
        Ok(rt) => Ok(rt.handle().clone()),
        Err(e) => Err(anyhow::anyhow!("failed to start the extension I/O runtime: {e}")),
    }
}

/// Loads JavaScript / TypeScript extensions in the embedded runtime.
pub struct JsExtensionHost {
    bridge: Arc<Bridge>,
}

struct SharedHost {
    cwd: String,
    generation: u64,
    host: Arc<JsExtensionHost>,
}

static SHARED: LazyLock<Mutex<Option<SharedHost>>> = LazyLock::new(|| Mutex::new(None));
static GENERATION: AtomicU64 = AtomicU64::new(0);

impl JsExtensionHost {
    /// The host shared by extension loads for `cwd` in the current cache generation.
    pub async fn shared(cwd: &str) -> anyhow::Result<Arc<JsExtensionHost>> {
        let generation = GENERATION.load(Ordering::SeqCst);
        {
            let shared = SHARED.lock();
            if let Some(s) = &*shared
                && s.cwd == cwd
                && s.generation == generation
            {
                return Ok(s.host.clone());
            }
        }
        let host = Arc::new(JsExtensionHost::new(cwd).await?);
        let mut shared = SHARED.lock();
        if let Some(s) = &*shared
            && s.cwd == cwd
            && s.generation == generation
        {
            return Ok(s.host.clone());
        }
        if GENERATION.load(Ordering::SeqCst) == generation {
            *shared = Some(SharedHost { cwd: cwd.to_string(), generation, host: host.clone() });
        }
        Ok(host)
    }

    /// Forget the shared host: the next load starts a new JS runtime. Extensions loaded from the
    /// previous runtime keep it alive until they are dropped.
    pub fn reset_shared() {
        GENERATION.fetch_add(1, Ordering::SeqCst);
        *SHARED.lock() = None;
    }

    /// Start a new JS runtime with pi's virtual modules.
    pub async fn new(cwd: &str) -> anyhow::Result<JsExtensionHost> {
        let mut virtual_modules = HashMap::new();
        virtual_modules.insert(BRIDGE_SPECIFIER.to_string(), VirtualModule::EsmJs(BRIDGE_JS.to_string()));
        virtual_modules.insert("pi-rs:marked".to_string(), VirtualModule::EsmJs(MARKED_JS.to_string()));
        for (specifier, source) in TYPEBOX_MODULES {
            virtual_modules.insert(specifier.to_string(), VirtualModule::EsmJs(source.to_string()));
        }
        virtual_modules.insert("@earendil-works/pi-tui".to_string(), VirtualModule::EsmJs(PI_TUI_JS.to_string()));
        virtual_modules.insert("@earendil-works/pi-ai".to_string(), VirtualModule::EsmJs(PI_AI_JS.to_string()));
        virtual_modules.insert("pi-rs:ai-core".to_string(), VirtualModule::EsmJs(PI_AI_CORE_JS.to_string()));
        virtual_modules.insert("pi-rs:ai-faux".to_string(), VirtualModule::EsmJs(PI_AI_FAUX_JS.to_string()));
        virtual_modules.insert("pi-rs:ai-rust".to_string(), VirtualModule::EsmJs(PI_AI_RUST_JS.to_string()));
        virtual_modules.insert("pi-rs:sdk".to_string(), VirtualModule::EsmJs(PI_SDK_JS.to_string()));
        virtual_modules.insert("@earendil-works/pi-ai/models".to_string(), VirtualModule::EsmJs(PI_AI_MODELS_JS.to_string()));
        virtual_modules
            .insert("@earendil-works/pi-ai/providers/all".to_string(), VirtualModule::EsmJs(PI_AI_PROVIDERS_ALL_JS.to_string()));
        // Types only (OAuth declaration compatibility entry point).
        virtual_modules.insert("@earendil-works/pi-ai/oauth".to_string(), VirtualModule::EsmJs("export {};".to_string()));
        virtual_modules
            .insert("@earendil-works/pi-agent-core".to_string(), VirtualModule::EsmJs(PI_AGENT_CORE_JS.to_string()));
        virtual_modules
            .insert("@earendil-works/pi-coding-agent".to_string(), VirtualModule::EsmJs(PI_CODING_AGENT_JS.to_string()));
        let mut aliases = HashMap::new();
        for (from, to) in [
            ("@sinclair/typebox", "typebox"),
            ("@sinclair/typebox/value", "typebox/value"),
            ("@sinclair/typebox/compile", "typebox/compile"),
            ("@sinclair/typebox/compiler", "typebox/compile"),
            ("typebox/type", "typebox"),
            ("@earendil-works/pi-ai/compat", "@earendil-works/pi-ai"),

            ("@mariozechner/pi-coding-agent", "@earendil-works/pi-coding-agent"),
            ("@mariozechner/pi-agent-core", "@earendil-works/pi-agent-core"),
            ("@mariozechner/pi-tui", "@earendil-works/pi-tui"),
            ("@mariozechner/pi-ai", "@earendil-works/pi-ai"),
            ("@mariozechner/pi-ai/compat", "@earendil-works/pi-ai"),
            ("@mariozechner/pi-ai/models", "@earendil-works/pi-ai/models"),
            ("@mariozechner/pi-ai/oauth", "@earendil-works/pi-ai/oauth"),
            ("@mariozechner/pi-ai/providers/all", "@earendil-works/pi-ai/providers/all"),
        ] {
            aliases.insert(from.to_string(), to.to_string());
        }
        let options = RuntimeOptions {
            cwd: Some(std::path::PathBuf::from(cwd)),
            virtual_modules,
            aliases,
            thread_name: "pi-extensions".to_string(),
            tokio: Some(extension_io_runtime()?),
            ..Default::default()
        };
        let rt = JsRuntime::new(options).map_err(js_error)?;

        let (ui_tx, ui_rx) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let _ = std::thread::Builder::new().name("pi-ext-ui".to_string()).spawn(move || {
            while let Ok(f) = ui_rx.recv() {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            }
        });

        let bridge_slot: Arc<Mutex<Weak<Bridge>>> = Arc::new(Mutex::new(Weak::new()));
        let rt2 = rt.clone();
        let slot2 = bridge_slot.clone();
        let init = async move {
            // Renders hold the UI lock while they call into JS (extension components), so the JS
            // thread must never wait for that lock (e.g. a host call that emits session events).
            rt2.with_context(|_| pi_tui::ui_lock::exempt_current_thread()).await?;
            // And a thread holding the UI lock that waits for the JS thread (a frame rendering an
            // extension component, an atomic interactive task) releases it while it waits: the JS
            // thread may meanwhile wait for the extension UI executor, which needs the lock.
            pi_jsrt::set_blocking_wait_hook(|wait| pi_tui::ui_lock::unlocked(wait));
            let slot = slot2.clone();
            rt2.register_function("__piHost.call", move |rt, args| {
                let bridge = slot.lock().upgrade().ok_or_else(|| pi_jsrt::Error::host("pi extension host is shut down"))?;
                let op = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
                let payload = args.get(1).cloned().unwrap_or(Value::Null);
                api::dispatch(&bridge, rt, &op, payload).map_err(|e| pi_jsrt::Error::host(format!("{e:#}")))
            })
            .await?;
            let slot = slot2.clone();
            rt2.register_async_function("__piHost.callAsync", move |rt, args| {
                let bridge = slot.lock().upgrade();
                async move {
                    let bridge = bridge.ok_or_else(|| pi_jsrt::Error::host("pi extension host is shut down"))?;
                    let op = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
                    let payload = args.get(1).cloned().unwrap_or(Value::Null);
                    api::dispatch_async(bridge, rt, op, payload).await.map_err(|e| pi_jsrt::Error::host(format!("{e:#}")))
                }
            })
            .await?;
            // `@earendil-works/pi-durable[/…]` and `@earendil-works/chord[/…]`: the embedded packages
            // over native pi-durable/pi-chord (crates/pi-durable-jsrt; js/EXPORTS.md).
            pi_durable_jsrt::install(&rt2, pi_durable_jsrt::HostOptions::default()).await?;
            let glue = rt2.import(BRIDGE_SPECIFIER).await?;
            Ok::<JsHandle, pi_jsrt::Error>(glue)
        };
        let glue = init.await.map_err(js_error)?;

        let bridge = Arc::new(Bridge {
            rt: rt.clone(),
            glue,
            cwd: cwd.to_string(),
            apis: Registry::new(),
            unsubscribes: Registry::new(),
            contexts: Registry::new(),
            context_ids: Mutex::new(HashMap::new()),
            tokens: Registry::new(),
            callbacks: Registry::new(),
            async_callbacks: Registry::new(),
            callback_ids: Mutex::new(HashMap::new()),
            session_managers: Registry::new(),
            session_manager_ids: Mutex::new(HashMap::new()),
            streams: Registry::new(),
            components: component::ComponentRegistries::default(),
            builtins: Registry::new(),
            natives: Registry::new(),
            native_provider_ids: Mutex::new(HashMap::new()),
            origins: Origins::default(),
            settings_managers: Registry::new(),
            model_runtimes: Registry::new(),
            model_runtime_ids: Mutex::new(HashMap::new()),
            model_registries: Registry::new(),
            settings_manager_ids: Mutex::new(HashMap::new()),
            resource_loaders: Registry::new(),
            agent_sessions: Registry::new(),
            tool_hooks: Registry::new(),
            hook_contexts: Registry::new(),
            extension_refs: Registry::new(),
            extension_ids: Mutex::new(HashMap::new()),
            ext_runtimes: Registry::new(),
            ext_runtime_ids: Mutex::new(HashMap::new()),
            package_managers: Registry::new(),
            session_cleanup: Mutex::new(None),
            ui_tx: Mutex::new(ui_tx),
        });
        *bridge_slot.lock() = Arc::downgrade(&bridge);
        // pi's session teardown (`cleanupSessionResources()`) also runs cleanups registered from
        // JavaScript with pi-ai's `registerSessionResourceCleanup`.
        let weak = Arc::downgrade(&bridge);
        let unregister = pi_ai::session_resources::register_session_resource_cleanup(Arc::new(move |session_id| {
            if let Some(bridge) = weak.upgrade() {
                let args = vec![serde_json::json!(session_id)];
                // pi-ai's cleanup registry is process-wide, so disposing a session runs this for
                // every extension runtime. A session disposed from another runtime's host call runs
                // it on that runtime's JS thread; blocking that thread on this runtime deadlocks
                // when this runtime's JS thread is doing the same towards it (two hosts disposing
                // at once), so from another runtime's JS thread the cleanup is queued here and
                // runs in order with this runtime's other work. On this runtime's own JS thread,
                // or off any JS thread, it runs before teardown continues, as in pi.
                if !bridge.rt.is_js_thread() && pi_jsrt::ctx::with_current(|_| ()).is_some() {
                    let glue = bridge.glue.clone();
                    bridge.rt.tokio().spawn(async move {
                        let _ = glue.call_method("cleanupJsSessionResources", args).await;
                    });
                } else {
                    bridge.glue_call_blocking("cleanupJsSessionResources", args)?;
                }
            }
            Ok(())
        }));
        *bridge.session_cleanup.lock() = Some(Box::new(unregister));
        // pi-ai's glue defines the stream class used by host-backed streams. The entry point itself
        // also re-exports TypeBox's `Type`, which loads only when an extension imports pi-ai or TypeBox.
        rt.import("pi-rs:ai-rust").await.map_err(js_error)?;
        // `ctx.modelRegistry.runtime` hands out the SDK's ModelRuntime class.
        rt.import("pi-rs:sdk").await.map_err(js_error)?;
        Ok(JsExtensionHost { bridge })
    }

    /// The JS runtime of this host.
    pub fn runtime(&self) -> &JsRuntime {
        &self.bridge.rt
    }

    /// Import an extension module and return its factory, or `None` when the module does not
    /// export a function (default export or the module itself, like jiti `{ default: true }`).
    pub async fn import_factory(self: &Arc<Self>, path: &str) -> anyhow::Result<Option<ExtensionFactory>> {
        let rt = self.bridge.rt.clone();
        // jiti `moduleCache: false`: every import of an extension evaluates it again.
        rt.clear_module_cache().await.map_err(js_error)?;
        // Imported as a top-level module rather than through `import()` from the glue module:
        // Boa caches dynamic imports per referrer and would reject the re-evaluated module.
        let namespace = rt.import(path).await.map_err(js_error)?;
        let marker = self.bridge.glue_call("factoryFromModule", vec![namespace.to_value()]).await?;
        let Some(factory) = self.bridge.handle(&marker) else {
            return Ok(None);
        };
        let bridge = self.bridge.clone();
        let host = self.clone();
        Ok(Some(Arc::new(move |api: ExtensionAPIRef| -> BoxFuture<'static, anyhow::Result<()>> {
            let bridge = bridge.clone();
            let factory = factory.clone();
            let _host = host.clone();
            Box::pin(async move {
                let api_id = bridge.apis.insert(api);
                bridge.glue_call("runFactory", vec![factory.to_value(), json!(api_id)]).await?;
                Ok(())
            })
        })))
    }
}

