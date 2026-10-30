//! The in-process host: the `__piDurable` global the glue calls, the method registry
//! ([`Dispatch`] / [`Methods`]), the embedded module table, and helpers for calling JS back with
//! [`HostValue`]s.
//!
//! JS side (`js/bridge.js`, specifier `pi-durable:bridge`):
//!
//! ```js
//! import { call, callAsync, ref } from "pi-durable:bridge";
//! const text = call("output.sanitize", input);          // sync method, runs on the JS thread
//! const result = await callAsync("session.open", opts); // async method, runs on tokio
//! call("x.register", ref(plainObject));                 // force a handle for a plain object
//! ```
//!
//! Rust side: a group of methods implements [`Dispatch`] and is listed in
//! [`crate::dispatch::defaults`]:
//!
//! ```ignore
//! struct OutputDispatch;
//! impl Dispatch for OutputDispatch {
//!     fn register(&self, m: &mut Methods) {
//!         m.sync("output.sanitize", |_call, args| Ok(HostValue::String(sanitize(args.string(0)?))));
//!         m.asynchronous("output.flush", |_call, args| async move { Ok(HostValue::Undefined) });
//!     }
//! }
//! ```
//!
//! Sync methods run inside [`pi_jsrt::ctx::host_call`], so they may call JS back synchronously
//! ([`call_sync`], [`pi_jsrt::JsHandle::call_blocking`]) re-entrantly. Method closures must not
//! capture a [`JsRuntime`] (the global function would keep its own runtime alive); use
//! [`Call::runtime`].

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use boa_engine::object::FunctionObjectBuilder;
use boa_engine::property::PropertyDescriptor;
use boa_engine::{
    Context, JsArgs, JsObject, JsResult, JsString, JsValue, NativeFunction, js_string,
};
use futures::FutureExt;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use pi_json::strict::JsonString;
use pi_jsrt::{JsHandle, JsRuntime, VirtualModule};

use crate::errors::{ErrorClass, HostError, HostResult};
use crate::value::{self, HostValue, RefData};

mod table {
    include!(concat!(env!("OUT_DIR"), "/modules.rs"));
}

/// Name of the global object the bridge talks to.
pub const GLOBAL: &str = "__piDurable";
/// Specifier of the bridge module.
pub const BRIDGE: &str = "pi-durable:bridge";

// ---------------------------------------------------------------------------
// Modules
// ---------------------------------------------------------------------------

/// The specifier a glue file (path under `js/`) is served as:
///
/// - `bridge.js` → `pi-durable:bridge`, `vitest.js` → `vitest`;
/// - `internal/<p>.js` → `pi-durable:internal/<p>` (glue-private modules);
/// - `chord/<p>.js` → `@earendil-works/chord/<p>`, `<p>.js` → `@earendil-works/pi-durable/<p>`,
///   where a trailing `/index` is dropped (`chord/index.js` → `@earendil-works/chord`,
///   `env/index.js` → `@earendil-works/pi-durable/env`) and `package.json.js` serves
///   `<package>/package.json`;
/// - `pi-codemode/<p>.js` → `@earendil-works/pi-codemode/<p>` (same rules; the QuickJS-wasm
///   sandbox package cannot run in Boa, so it is embedded over the native `pi-codemode` crate);
/// - `pi-telemetry/<p>.js` → `@earendil-works/pi-telemetry/<p>` (same rules; over the native
///   `pi-telemetry` crate, so the embedded packages resolve nothing from `node_modules`).
pub fn specifier_for(path: &str) -> Option<String> {
    let stem = path.strip_suffix(".js")?;
    match stem {
        "bridge" => return Some(BRIDGE.to_string()),
        "vitest" => return Some("vitest".to_string()),
        _ => {}
    }
    if let Some(rest) = stem.strip_prefix("internal/") {
        return Some(format!("pi-durable:internal/{rest}"));
    }
    let (package, rest) = match stem.strip_prefix("chord/") {
        Some(rest) => ("@earendil-works/chord", rest),
        None if stem == "chord" => ("@earendil-works/chord", ""),
        None => match stem.strip_prefix("pi-codemode/") {
            Some(rest) => ("@earendil-works/pi-codemode", rest),
            None => match stem.strip_prefix("pi-telemetry/") {
                Some(rest) => ("@earendil-works/pi-telemetry", rest),
                None => ("@earendil-works/pi-durable", stem),
            },
        },
    };
    let rest = rest.strip_suffix("/index").unwrap_or(rest);
    let rest = if rest == "index" { "" } else { rest };
    Some(if rest.is_empty() {
        package.to_string()
    } else {
        format!("{package}/{rest}")
    })
}

/// Every embedded module as `(specifier, source)`; `vitest` only when `include_vitest`.
pub fn modules(include_vitest: bool) -> Vec<(String, &'static str)> {
    table::MODULES
        .iter()
        .filter_map(|(path, src)| specifier_for(path).map(|spec| (spec, *src)))
        .filter(|(spec, _)| include_vitest || spec != "vitest")
        .collect()
}

/// Every embedded module as [`VirtualModule`]s, for [`pi_jsrt::RuntimeOptions::virtual_modules`].
pub fn virtual_modules(include_vitest: bool) -> HashMap<String, VirtualModule> {
    modules(include_vitest)
        .into_iter()
        .map(|(spec, src)| (spec, VirtualModule::EsmJs(src.to_string())))
        .collect()
}

// ---------------------------------------------------------------------------
// Methods
// ---------------------------------------------------------------------------

/// Arguments of a host method call.
#[derive(Debug, Default)]
pub struct Args(pub Vec<HostValue>);

const UNDEFINED: HostValue = HostValue::Undefined;

impl Args {
    /// Number of arguments passed.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no argument was passed.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Argument `i` (`undefined` when missing).
    pub fn get(&self, i: usize) -> &HostValue {
        self.0.get(i).unwrap_or(&UNDEFINED)
    }

    /// Take argument `i`, leaving `undefined`.
    pub fn take(&mut self, i: usize) -> HostValue {
        self.0.get_mut(i).map(std::mem::take).unwrap_or_default()
    }

    fn expected(i: usize, what: &str) -> HostError {
        HostError::type_error(format!("argument {i} must be {what}"))
    }

    /// Argument `i` as a string (UTF-16).
    pub fn string(&self, i: usize) -> HostResult<&JsonString> {
        self.get(i)
            .as_string()
            .ok_or_else(|| Args::expected(i, "a string"))
    }

    /// Argument `i` as Rust text (lone surrogates become U+FFFD).
    pub fn text(&self, i: usize) -> HostResult<String> {
        self.string(i).map(JsonString::to_string_lossy)
    }

    /// Argument `i` as a number.
    pub fn f64(&self, i: usize) -> HostResult<f64> {
        self.get(i)
            .as_f64()
            .ok_or_else(|| Args::expected(i, "a number"))
    }

    /// Argument `i` as a boolean.
    pub fn bool(&self, i: usize) -> HostResult<bool> {
        self.get(i)
            .as_bool()
            .ok_or_else(|| Args::expected(i, "a boolean"))
    }

    /// Argument `i` as a JS handle (function or object).
    pub fn handle(&self, i: usize) -> HostResult<JsHandle> {
        self.get(i)
            .as_handle()
            .cloned()
            .ok_or_else(|| Args::expected(i, "a function or object"))
    }

    /// Argument `i` as native state of type `T` (see [`crate::NativeRef`]).
    pub fn native<T: std::any::Any + Send + Sync>(&self, i: usize) -> HostResult<Arc<T>> {
        self.get(i)
            .as_native::<T>()
            .ok_or_else(|| Args::expected(i, "a host object of the right kind"))
    }
}

/// The context of one host method call.
#[derive(Clone)]
pub struct Call {
    rt: JsRuntime,
    method: Arc<str>,
}

impl Call {
    /// The runtime the call came from.
    pub fn runtime(&self) -> &JsRuntime {
        &self.rt
    }

    /// The method name.
    pub fn method(&self) -> &str {
        &self.method
    }
}

type SyncMethod = Arc<dyn Fn(&Call, Args) -> HostResult<HostValue> + Send + Sync>;
type AsyncMethod =
    Arc<dyn Fn(Call, Args) -> BoxFuture<'static, HostResult<HostValue>> + Send + Sync>;

#[derive(Clone)]
enum Method {
    Sync(SyncMethod),
    Async(AsyncMethod),
}

/// The method table [`Dispatch`] implementations register into.
#[derive(Default)]
pub struct Methods {
    map: HashMap<String, Method>,
}

impl Methods {
    fn insert(&mut self, name: &str, method: Method) {
        assert!(
            self.map.insert(name.to_string(), method).is_none(),
            "pi-durable host method {name} registered twice"
        );
    }

    /// Register a synchronous method (runs on the JS thread; JS calls it with `call`).
    pub fn sync<F>(&mut self, name: &str, f: F) -> &mut Self
    where
        F: Fn(&Call, Args) -> HostResult<HostValue> + Send + Sync + 'static,
    {
        self.insert(name, Method::Sync(Arc::new(f)));
        self
    }

    /// Register an asynchronous method (runs on tokio; JS calls it with `callAsync`).
    pub fn asynchronous<F, Fut>(&mut self, name: &str, f: F) -> &mut Self
    where
        F: Fn(Call, Args) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HostResult<HostValue>> + Send + 'static,
    {
        self.insert(
            name,
            Method::Async(Arc::new(move |call, args| f(call, args).boxed())),
        );
        self
    }

    /// Whether `name` is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }
}

/// A group of host methods (one per `src/dispatch/*.rs` module).
pub trait Dispatch: Send + Sync + 'static {
    /// Register this group's methods.
    fn register(&self, methods: &mut Methods);
}

/// Options for [`install`].
pub struct HostOptions {
    /// Also serve the `vitest` shim (`js/vitest.js`).
    pub vitest: bool,
    /// Method groups; defaults to [`crate::dispatch::defaults`].
    pub dispatch: Vec<Arc<dyn Dispatch>>,
}

impl Default for HostOptions {
    fn default() -> Self {
        HostOptions {
            vitest: false,
            dispatch: crate::dispatch::defaults(),
        }
    }
}

struct Registry {
    methods: HashMap<String, Method>,
    counters: Mutex<BTreeMap<String, u64>>,
}

impl Registry {
    fn lookup(&self, name: &str) -> Option<Method> {
        let method = self.methods.get(name).cloned();
        if method.is_some() {
            *self.counters.lock().entry(name.to_string()).or_default() += 1;
        }
        method
    }
}

/// An installed host (cheap to clone).
#[derive(Clone)]
pub struct DurableHost {
    rt: JsRuntime,
    registry: Arc<Registry>,
}

impl DurableHost {
    /// The runtime the host is installed in.
    pub fn runtime(&self) -> &JsRuntime {
        &self.rt
    }

    /// Calls per method so far (proof that execution reached Rust).
    pub fn counters(&self) -> BTreeMap<String, u64> {
        self.registry.counters.lock().clone()
    }

    /// Total method calls so far.
    pub fn total_calls(&self) -> u64 {
        self.registry.counters.lock().values().sum()
    }

    /// Registered method names, sorted.
    pub fn methods(&self) -> Vec<String> {
        let mut names: Vec<String> = self.registry.methods.keys().cloned().collect();
        names.sort();
        names
    }
}

impl std::fmt::Debug for DurableHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableHost")
            .field("methods", &self.registry.methods.len())
            .finish()
    }
}

/// Install the host into `rt`: the embedded modules (as virtual modules, so install before the
/// first import of any of them) and the `__piDurable` global.
pub async fn install(rt: &JsRuntime, options: HostOptions) -> pi_jsrt::Result<DurableHost> {
    for (spec, module) in virtual_modules(options.vitest) {
        rt.set_virtual_module(spec, module);
    }
    let mut methods = Methods::default();
    for group in &options.dispatch {
        group.register(&mut methods);
    }
    if options.vitest {
        crate::dispatch::vitest::VitestDispatch.register(&mut methods);
    }
    let registry = Arc::new(Registry {
        methods: methods.map,
        counters: Mutex::default(),
    });
    let r = registry.clone();
    rt.with_context(move |ctx| install_global(ctx, r).map_err(|e| e.to_string()))
        .await?
        .map_err(pi_jsrt::Error::host)?;
    Ok(DurableHost {
        rt: rt.clone(),
        registry,
    })
}

fn function(ctx: &mut Context, name: &str, length: usize, f: NativeFunction) -> JsObject {
    let realm = ctx.realm().clone();
    FunctionObjectBuilder::new(&realm, f)
        .name(JsString::from(name))
        .length(length)
        .build()
        .into()
}

fn install_global(ctx: &mut Context, registry: Arc<Registry>) -> JsResult<()> {
    let raw = JsObject::with_object_proto(ctx.intrinsics());
    let r = registry.clone();
    // SAFETY: the closures capture only an `Arc<Registry>` (no GC-managed values).
    let call =
        unsafe { NativeFunction::from_closure(move |_, args, ctx| raw_call(ctx, &r, args, false)) };
    let r = registry.clone();
    let call_async =
        unsafe { NativeFunction::from_closure(move |_, args, ctx| raw_call(ctx, &r, args, true)) };
    let r = registry;
    let has = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let name = args
                .get_or_undefined(0)
                .to_string(ctx)?
                .to_std_string_lossy();
            Ok(match r.methods.get(&name) {
                Some(Method::Sync(_)) => js_string!("sync").into(),
                Some(Method::Async(_)) => js_string!("async").into(),
                None => JsValue::undefined(),
            })
        })
    };
    let entries = [
        ("call", call),
        ("callAsync", call_async),
        ("method", has),
        ("ref", NativeFunction::from_fn_ptr(op_ref)),
    ];
    for (name, f) in entries {
        let f = function(ctx, name, 1, f);
        raw.define_property_or_throw(
            JsString::from(name),
            PropertyDescriptor::builder()
                .value(f)
                .writable(false)
                .enumerable(true)
                .configurable(false),
            ctx,
        )?;
    }
    ctx.global_object().define_property_or_throw(
        JsString::from(GLOBAL),
        PropertyDescriptor::builder()
            .value(raw)
            .writable(false)
            .enumerable(false)
            .configurable(true),
        ctx,
    )?;
    Ok(())
}

/// `__piDurable.ref(value)`: `value` crosses as a handle even when it is plain data.
fn op_ref(_: &JsValue, args: &[JsValue], _ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsObject::from_proto_and_data(None, RefData(args.get_or_undefined(0).clone())).into())
}

fn panic_error(method: &str) -> HostError {
    HostError::new(
        ErrorClass::Error,
        format!("pi-durable host method {method} panicked"),
    )
}

fn raw_call(
    ctx: &mut Context,
    registry: &Registry,
    args: &[JsValue],
    is_async: bool,
) -> JsResult<JsValue> {
    let name = args
        .get_or_undefined(0)
        .to_string(ctx)?
        .to_std_string_lossy();
    let rt = pi_jsrt::ctx::runtime(ctx)
        .ok_or_else(|| boa_engine::JsNativeError::typ().with_message("runtime is shutting down"))?;
    let fail = |ctx: &mut Context, e: HostError| -> JsResult<JsValue> {
        if is_async {
            let reason = e.to_js(ctx);
            Ok(boa_engine::object::builtins::JsPromise::reject(
                boa_engine::JsError::from_opaque(reason),
                ctx,
            )?
            .into())
        } else {
            Err(e.to_boa(ctx))
        }
    };
    let Some(method) = registry.lookup(&name) else {
        return fail(
            ctx,
            HostError::type_error(format!("unknown pi-durable host method {name}")),
        );
    };
    let mut converted = Vec::with_capacity(args.len().saturating_sub(1));
    for a in args.iter().skip(1) {
        match value::from_js(ctx, a) {
            Ok(v) => converted.push(v),
            Err(e) => return fail(ctx, e),
        }
    }
    let call = Call {
        rt,
        method: Arc::from(name.as_str()),
    };
    match method {
        Method::Sync(f) => {
            let r = pi_jsrt::ctx::host_call(ctx, || {
                std::panic::catch_unwind(AssertUnwindSafe(|| f(&call, Args(converted))))
            });
            let r = r.unwrap_or_else(|_| Err(panic_error(&name)));
            match r {
                Ok(v) if is_async => {
                    let v = value::to_js(ctx, &v)?;
                    Ok(boa_engine::object::builtins::JsPromise::resolve(v, ctx)?.into())
                }
                Ok(v) => value::to_js(ctx, &v),
                Err(e) => fail(ctx, e),
            }
        }
        Method::Async(_) if !is_async => fail(
            ctx,
            HostError::type_error(format!(
                "pi-durable host method {name} is asynchronous; use callAsync"
            )),
        ),
        Method::Async(f) => {
            let fut = AssertUnwindSafe(f(call, Args(converted))).catch_unwind();
            Ok(pi_jsrt::ctx::spawn_promise(ctx, async move {
                let r = fut.await.unwrap_or_else(|_| Err(panic_error(&name)));
                Box::new(move |ctx: &mut Context| match r {
                    Ok(v) => value::to_js(ctx, &v),
                    Err(e) => Err(e.to_boa(ctx)),
                }) as pi_jsrt::ctx::Settle
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// Calling JS from Rust with host values
// ---------------------------------------------------------------------------

fn invoke(
    ctx: &mut Context,
    target: &JsHandle,
    method: Option<&str>,
    args: &[HostValue],
) -> JsResult<JsValue> {
    let value = pi_jsrt::ctx::handle_value(ctx, target).ok_or_else(|| {
        boa_engine::JsNativeError::typ().with_message(format!("invalid JS handle {}", target.id()))
    })?;
    let mut js_args = Vec::with_capacity(args.len());
    for a in args {
        js_args.push(value::to_js(ctx, a)?);
    }
    match method {
        None => {
            let f = value.as_callable().ok_or_else(|| {
                boa_engine::JsNativeError::typ().with_message("handle is not a function")
            })?;
            f.call(&JsValue::undefined(), &js_args, ctx)
        }
        Some(name) => {
            let obj = value.to_object(ctx)?;
            let f = obj.get(JsString::from(name), ctx)?;
            let f = f.as_callable().ok_or_else(|| {
                boa_engine::JsNativeError::typ().with_message(format!("{name} is not a function"))
            })?;
            f.call(&value, &js_args, ctx)
        }
    }
}

fn settled(ctx: &mut Context, r: Result<JsValue, JsValue>) -> HostResult<HostValue> {
    match r {
        Ok(v) => value::from_js(ctx, &v),
        Err(e) => Err(HostError::from_js(ctx, &e)),
    }
}

fn runtime_error(e: pi_jsrt::Error) -> HostError {
    HostError::new(ErrorClass::Error, e.to_string())
}

/// Call a JS function (`method: None`) or `target[method](...)` and wait for the result,
/// awaiting a returned promise.
///
/// On the JS thread this works re-entrantly from inside a sync host method (a returned promise
/// must settle from its pending jobs, else this fails); on other threads it blocks until the
/// result settles.
pub fn call_sync(
    target: &JsHandle,
    method: Option<&str>,
    args: Vec<HostValue>,
) -> HostResult<HostValue> {
    let rt = target.runtime().clone();
    if rt.is_js_thread() {
        let r = pi_jsrt::ctx::with_current(|ctx| {
            let r = invoke(ctx, target, method, &args).map_err(|e| HostError::from_boa(ctx, e))?;
            let Some(promise) = r.as_promise() else {
                return value::from_js(ctx, &r);
            };
            pi_jsrt::ctx::run_jobs(ctx);
            match promise.state() {
                boa_engine::builtins::promise::PromiseState::Fulfilled(v) => {
                    value::from_js(ctx, &v)
                }
                boa_engine::builtins::promise::PromiseState::Rejected(e) => {
                    Err(HostError::from_js(ctx, &e))
                }
                boa_engine::builtins::promise::PromiseState::Pending => Err(HostError::new(
                    ErrorClass::Error,
                    "a synchronous call into JS returned a promise that is still pending",
                )),
            }
        });
        return r.unwrap_or_else(|| Err(runtime_error(pi_jsrt::Error::Deadlock)));
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let target = target.clone();
    let method = method.map(str::to_string);
    rt.with_context_blocking(move |ctx| {
        let r = invoke(ctx, &target, method.as_deref(), &args);
        pi_jsrt::ctx::when_settled(ctx, r, move |ctx, r| {
            let _ = tx.send(settled(ctx, r));
        });
    })
    .map_err(runtime_error)?;
    rx.recv()
        .unwrap_or_else(|_| Err(runtime_error(pi_jsrt::Error::Terminated)))
}

/// Async [`call_sync`]: queue the call on the JS thread and await the settled result.
pub async fn call_async(
    target: &JsHandle,
    method: Option<&str>,
    args: Vec<HostValue>,
) -> HostResult<HostValue> {
    let rt = target.runtime().clone();
    let (tx, rx) = futures::channel::oneshot::channel();
    let target = target.clone();
    let method = method.map(str::to_string);
    rt.with_context(move |ctx| {
        let r = invoke(ctx, &target, method.as_deref(), &args);
        pi_jsrt::ctx::when_settled(ctx, r, move |ctx, r| {
            let _ = tx.send(settled(ctx, r));
        });
    })
    .await
    .map_err(runtime_error)?;
    rx.await
        .unwrap_or_else(|_| Err(runtime_error(pi_jsrt::Error::Terminated)))
}
