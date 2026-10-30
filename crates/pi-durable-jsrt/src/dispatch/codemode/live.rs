//! The sandboxes of `@earendil-works/pi-codemode` (`js/pi-codemode/index.js`) over the native
//! `pi_codemode::CodemodeSandbox`: `CodemodeSandbox` and `InlineCodemodeSandbox` start a Boa VM on
//! its own thread (the inline one with an interrupt budget, as upstream's in-thread QuickJS),
//! `RemoteCodemodeSandbox` keeps the host role and reaches the caller's JS `remote` for the VM,
//! and `serveCodemodeRemote` is the remote side. Tools, globals, admission callbacks and remotes
//! stay JS: the glue's adapters are called back with the caller's objects.
//!
//! The VM protocol and results cross as JSON through `serde_json` (the native host is UTF-8, as
//! in upstream's `JSON.stringify`/`JSON.parse` bridge); `start` runs keep every UTF-16 code unit
//! (`sdk.rs`).

use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use pi_codemode::{
    AbortController, AbortSignal, CodemodeExecuteOptions, CodemodeRemote, CodemodeRemoteExchange,
    CodemodeRemoteMessage, CodemodeRemoteReply, CodemodeRemoteRequest, CodemodeSandbox,
    CodemodeSandboxOptions, CodemodeServeOptions, CodemodeTool, CodemodeToolDeclaration,
    RemoteCodemodeSandboxOptions, serve_codemode_remote,
};
use pi_json::strict;
use pi_jsrt::JsHandle;

use super::sdk::Sandbox;
use crate::errors::{ErrorClass, HostError, HostResult};
use crate::host::{Args, Call, Methods, call_async, call_sync};
use crate::value::HostValue;

/// Upstream `InlineCodemodeSandbox`'s default interrupt budget.
pub const INLINE_INTERRUPT_BUDGET: u64 = 100_000;

fn field<'a>(value: &'a HostValue, key: &str) -> &'a HostValue {
    value.get(key).unwrap_or(&HostValue::Undefined)
}

fn count(value: &HostValue) -> Option<u64> {
    value.as_f64().map(|n| if n.is_nan() || n <= 0.0 { 0 } else { n.min(u64::MAX as f64) as u64 })
}

fn handle(value: &HostValue, name: &str) -> HostResult<JsHandle> {
    value
        .as_handle()
        .cloned()
        .ok_or_else(|| HostError::type_error(format!("{name} must be a function")))
}

fn message(error: &HostError) -> String {
    error.message().to_string_lossy()
}

/// A serde value as a JS value (through strict JSON).
fn to_host<T: serde::Serialize>(value: &T) -> HostResult<HostValue> {
    let text = serde_json::to_string(value).map_err(|e| HostError::type_error(e.to_string()))?;
    let value = strict::parse(&text).map_err(HostError::type_error)?;
    Ok(HostValue::from_strict(&value))
}

/// A JS value as a serde value (`JSON.stringify` semantics).
fn from_host<T: serde::de::DeserializeOwned>(value: &HostValue, what: &str) -> HostResult<T> {
    let text = strict::stringify(&value.to_strict_omitting_undefined()?);
    serde_json::from_str(&text).map_err(|e| HostError::type_error(format!("invalid {what}: {e}")))
}

/// The JS state a sandbox calls back into.
#[derive(Clone)]
pub(super) struct Callbacks {
    /// `(tool, argsJson, abort) => Promise<{ ok: true, json? } | { ok: false, message }>`.
    invoke: JsHandle,
    calls: LiveCalls,
}

/// Abort registration of one tool call: the JS `abort()` of its `AbortController`, run once its
/// native signal fires.
#[derive(Clone)]
struct CallAbort {
    slot: Arc<Mutex<Option<JsHandle>>>,
    signal: AbortSignal,
}

impl CallAbort {
    fn take_if_aborted(&self) -> Option<JsHandle> {
        if self.signal.aborted() { self.slot.lock().take() } else { None }
    }
}

/// The tool calls of one sandbox still running. Upstream's `finish()` aborts a call's controller
/// before the execution settles; the native signal fires on another thread, so `execute` delivers
/// every abort that is due before its result reaches JS (`flush`).
#[derive(Clone, Default)]
struct LiveCalls(Arc<Mutex<Vec<CallAbort>>>);

impl LiveCalls {
    fn remove(&self, call: &CallAbort) {
        self.0.lock().retain(|other| !Arc::ptr_eq(&other.slot, &call.slot));
    }

    async fn flush(&self) {
        let due: Vec<JsHandle> = {
            let mut calls = self.0.lock();
            let due = calls.iter().filter_map(CallAbort::take_if_aborted).collect();
            calls.retain(|call| !call.signal.aborted());
            due
        };
        for abort in due {
            let _ = call_async(&abort, None, vec![]).await;
        }
    }
}

/// A JS `CodemodeTool` as a native tool: `execute(args, { signal })` runs through the glue's
/// `invoke` adapter, which mirrors upstream's host (`JSON.parse` of the arguments, `JSON.stringify`
/// of the value, `errorMessage` of a throw).
fn js_tool(declaration: &HostValue, tool: JsHandle, callbacks: &Callbacks) -> HostResult<CodemodeTool> {
    let declaration: CodemodeToolDeclaration = from_host(declaration, "tool declaration")?;
    let invoke = callbacks.invoke.clone();
    let calls = callbacks.calls.clone();
    Ok(CodemodeTool::from_declaration(
        declaration,
        Arc::new(move |args, context| {
            let invoke = invoke.clone();
            let tool = tool.clone();
            let calls = calls.clone();
            Box::pin(async move {
                let abort = CallAbort {
                    slot: Arc::default(),
                    signal: AbortSignal::from(&context.signal),
                };
                calls.0.lock().push(abort.clone());
                let token = HostValue::native(abort.clone());
                let args = match args {
                    None => HostValue::Undefined,
                    Some(value) => HostValue::string(&value.to_string()),
                };
                let call = call_async(&invoke, None, vec![HostValue::Handle(tool), args, token]);
                let watched = abort.clone();
                let cancel = async move {
                    watched.signal.cancelled().await;
                    let taken = watched.slot.lock().take();
                    if let Some(abort) = taken {
                        let _ = call_async(&abort, None, vec![]).await;
                    }
                    futures::future::pending::<()>().await;
                };
                let reply = match futures::future::select(Box::pin(call), Box::pin(cancel)).await {
                    futures::future::Either::Left((reply, _)) => reply,
                    futures::future::Either::Right(_) => unreachable!("the cancel future never ends"),
                };
                calls.remove(&abort);
                let reply = reply.map_err(|e| message(&e))?;
                if field(&reply, "ok").as_bool() != Some(true) {
                    return Err(field(&reply, "message")
                        .as_string()
                        .map(|m| m.to_string_lossy())
                        .unwrap_or_default());
                }
                match field(&reply, "json").as_string() {
                    None => Ok(None),
                    Some(json) => serde_json::from_str(&json.to_string_lossy())
                        .map(Some)
                        .map_err(|e| e.to_string()),
                }
            })
        }),
    ))
}

/// `[{ declaration, tool }]` from the glue.
fn js_tools(value: &HostValue, callbacks: &Callbacks) -> HostResult<Vec<CodemodeTool>> {
    match value {
        HostValue::Undefined => Ok(Vec::new()),
        HostValue::Array(items) => items
            .iter()
            .map(|item| js_tool(field(item, "declaration"), handle(field(item, "tool"), "execute")?, callbacks))
            .collect(),
        _ => Err(HostError::type_error("tools must be an array")),
    }
}

/// The caller's remote, picked once per execution by the glue's `remote()` wrapper.
struct JsRemote {
    /// `(remote, request, exchange) => remote.execute(request, messages => ...)`.
    execute: JsHandle,
    remote: JsHandle,
}

/// A native exchange handed to the caller's remote.
struct Exchange(CodemodeRemoteExchange);

impl CodemodeRemote for JsRemote {
    fn execute(&self, request: CodemodeRemoteRequest, exchange: CodemodeRemoteExchange) -> BoxFuture<'static, Result<(), String>> {
        let execute = self.execute.clone();
        let remote = self.remote.clone();
        async move {
            let request = to_host(&request).map_err(|e| message(&e))?;
            let exchange = HostValue::native(Exchange(exchange));
            call_async(&execute, None, vec![HostValue::Handle(remote), request, exchange])
                .await
                .map(|_| ())
                .map_err(|e| message(&e))
        }
        .boxed()
    }
}

/// Admission releases started by VM thread exits and not finished yet.
#[derive(Default)]
struct Releases {
    pending: Mutex<usize>,
    done: tokio::sync::Notify,
}

/// The `exited` promise of `onWorkerStarted`.
struct WorkerExit {
    exited: pi_codemode::WorkerExited,
    releases: Arc<Releases>,
}

fn sandbox_error(e: pi_codemode::CodemodeSandboxError) -> HostError {
    HostError::new(ErrorClass::Error, e.to_string())
}

/// `codemode.sdk.sandboxNew({ kind, timeoutMs, memoryLimitBytes, interruptBudget, tools, globals,
/// invoke, acquireWorker, onWorkerStarted, remote, remoteExecute })`.
pub(super) fn sandbox_new(_call: &Call, args: Args) -> HostResult<HostValue> {
    let options = args.get(0);
    let callbacks = Callbacks {
        invoke: handle(field(options, "invoke"), "invoke")?,
        calls: LiveCalls::default(),
    };
    let tools = js_tools(field(options, "tools"), &callbacks)?;
    let globals = js_tools(field(options, "globals"), &callbacks)?;
    let timeout_ms = field(options, "timeoutMs").as_f64();
    let memory_limit_bytes = count(field(options, "memoryLimitBytes"));
    let kind = field(options, "kind").as_string().map(|k| k.to_string_lossy()).unwrap_or_default();
    let sandbox = match kind.as_str() {
        "remote" => {
            let factory = handle(field(options, "remote"), "remote")?;
            let execute = handle(field(options, "remoteExecute"), "remoteExecute")?;
            let mut remote = RemoteCodemodeSandboxOptions::new(move || {
                // `remote()` throwing is "Failed to reach the remote sandbox: …".
                let remote = call_sync(&factory, None, vec![]).map_err(|e| message(&e))?;
                let remote = remote.as_handle().cloned().ok_or("remote() must return an object")?;
                Ok(Arc::new(JsRemote { execute: execute.clone(), remote }) as Arc<dyn CodemodeRemote>)
            });
            remote.tools = tools;
            remote.globals = globals;
            remote.timeout_ms = timeout_ms;
            remote.memory_limit_bytes = memory_limit_bytes;
            remote.interrupt_budget = count(field(options, "interruptBudget"));
            CodemodeSandbox::remote(remote)
        }
        "inline" | "thread" => {
            let mut native = CodemodeSandboxOptions {
                tools,
                globals,
                timeout_ms,
                memory_limit_bytes,
                interrupt_budget: count(field(options, "interruptBudget")),
                ..Default::default()
            };
            if kind == "inline" && native.interrupt_budget.is_none() {
                native.interrupt_budget = Some(INLINE_INTERRUPT_BUDGET);
            }
            // Upstream releases admission on the worker's `exit` before the `exited` promise of
            // `onWorkerStarted` settles; `releases` keeps that order across the JS callbacks.
            let releases = Arc::new(Releases::default());
            if let Some(acquire) = field(options, "acquireWorker").as_handle().cloned() {
                let releases = releases.clone();
                native.acquire_worker = Some(Arc::new(move || {
                    // Throwing denies admission; the returned function releases it.
                    let release = call_sync(&acquire, None, vec![]).map_err(|e| message(&e))?;
                    let release = release.as_handle().cloned();
                    let releases = releases.clone();
                    Ok(Box::new(move || {
                        if let Some(release) = release {
                            *releases.pending.lock() += 1;
                            let rt = release.runtime().clone();
                            rt.tokio().spawn(async move {
                                let _ = call_async(&release, None, vec![]).await;
                                *releases.pending.lock() -= 1;
                                releases.done.notify_waiters();
                            });
                        }
                    }) as pi_codemode::WorkerRelease)
                }));
            }
            if let Some(started) = field(options, "onWorkerStarted").as_handle().cloned() {
                native.on_worker_started = Some(Arc::new(move |exited: pi_codemode::WorkerExited| {
                    let exited = HostValue::native(WorkerExit { exited, releases: releases.clone() });
                    call_sync(&started, None, vec![exited]).map(|_| ()).map_err(|e| message(&e))
                }));
            }
            CodemodeSandbox::new(native)
        }
        _ => return Err(HostError::type_error(format!("unknown sandbox kind {kind:?}"))),
    }
    .map_err(sandbox_error)?;
    Ok(HostValue::native(Sandbox::live(sandbox, callbacks)))
}

pub(super) fn register(m: &mut Methods) {
    m.sync("codemode.sdk.registerTool", |_call, args| {
        let sandbox = args.native::<Sandbox>(0)?;
        let callbacks = sandbox.callbacks().cloned().ok_or_else(|| HostError::type_error("sandbox has no tool adapter"))?;
        let tool = js_tool(args.get(1), args.handle(2)?, &callbacks)?;
        sandbox.0.register_tool(tool).map_err(sandbox_error)?;
        Ok(HostValue::Undefined)
    });
    m.sync("codemode.sdk.unregisterTool", |_call, args| {
        let sandbox = args.native::<Sandbox>(0)?;
        Ok(HostValue::Bool(sandbox.0.unregister_tool(&args.text(1)?)))
    });
    m.sync("codemode.sdk.onAbort", |_call, args| {
        // `true`: the call was cancelled already, so the glue aborts it at once.
        let abort = args.native::<CallAbort>(0)?;
        if abort.signal.aborted() {
            return Ok(HostValue::Bool(true));
        }
        *abort.slot.lock() = Some(args.handle(1)?);
        Ok(HostValue::Bool(false))
    });
    // `execute(sandbox, code, { timeoutMs, store: { key: json }, aborted, reason }, signalRegistration)`.
    m.sync("codemode.sdk.controller", |_call, _args| Ok(HostValue::native(AbortController::new())));
    m.sync("codemode.sdk.controllerAbort", |_call, args| {
        let controller = args.native::<AbortController>(0)?;
        match args.get(1).as_string() {
            Some(reason) => controller.abort_with_reason(reason.to_string_lossy()),
            None => controller.abort(),
        }
        Ok(HostValue::Undefined)
    });
    m.asynchronous("codemode.sdk.execute", |_call, args| async move {
        let sandbox = args.native::<Sandbox>(0)?;
        let code = args.string(1)?.to_string_lossy();
        let options = args.get(2);
        let store = match field(options, "store") {
            HostValue::Undefined => None,
            HostValue::Object(entries) => Some(
                entries
                    .iter()
                    .map(|(key, json)| {
                        let json = json.as_string().map(|j| j.to_string_lossy()).unwrap_or_default();
                        Ok((key.to_string_lossy(), serde_json::from_str(&json).map_err(|e| HostError::type_error(e.to_string()))?))
                    })
                    .collect::<HostResult<serde_json::Map<_, _>>>()?,
            ),
            _ => return Err(HostError::type_error("store must be an object")),
        };
        let signal = match args.get(3) {
            HostValue::Undefined => None,
            _ => Some(args.native::<AbortController>(3)?.signal()),
        };
        let result = sandbox
            .0
            .execute(
                &code,
                CodemodeExecuteOptions {
                    signal,
                    timeout_ms: field(options, "timeoutMs").as_f64(),
                    store,
                    // The worker's timezone is the process's (`process.env.TZ`), as in Node.
                    timezone: field(options, "timezone").as_string().map(|tz| tz.to_string_lossy()),
                },
            )
            .await;
        if let Some(callbacks) = sandbox.callbacks() {
            callbacks.calls.flush().await;
        }
        to_host(&result.map_err(sandbox_error)?)
    });
    m.asynchronous("codemode.sdk.workerExited", |_call, args| async move {
        let exit = args.native::<WorkerExit>(0)?;
        exit.exited.clone().await;
        // Then every admission release the exit started.
        loop {
            let done = exit.releases.done.notified();
            let mut done = std::pin::pin!(done);
            done.as_mut().enable();
            if *exit.releases.pending.lock() == 0 {
                break;
            }
            done.await;
        }
        Ok(HostValue::Undefined)
    });
    // The client side of a remote: the caller's remote answers the VM's messages through this.
    // The exchange takes the messages now, on the JS thread, so exchanges reach the client in the
    // order the remote made them (output stays ahead of the call or result that follows it).
    m.asynchronous("codemode.sdk.exchange", |_call, args| {
        let started = (|| {
            let exchange = args.native::<Exchange>(0)?;
            let messages: Vec<CodemodeRemoteMessage> = from_host(args.get(1), "remote messages")?;
            Ok::<_, HostError>((exchange.0)(messages))
        })();
        async move {
            let reply = started?.await.map_err(|e| HostError::new(ErrorClass::Error, e))?;
            to_host(&reply)
        }
    });
    // `serveCodemodeRemote(request, exchange, options)`: the remote side, calling `exchange` back.
    m.asynchronous("codemode.sdk.serve", |_call, args| async move {
        let request: CodemodeRemoteRequest = from_host(args.get(0), "remote request")?;
        let exchange_fn = args.handle(1)?;
        let options = args.get(2);
        let exchange: CodemodeRemoteExchange = Arc::new(move |messages: Vec<CodemodeRemoteMessage>| {
            let exchange_fn = exchange_fn.clone();
            async move {
                let messages = to_host(&messages).map_err(|e| message(&e))?;
                let reply = call_async(&exchange_fn, None, vec![messages]).await.map_err(|e| message(&e))?;
                if reply.is_undefined() {
                    return Ok(CodemodeRemoteReply::None);
                }
                from_host::<CodemodeRemoteReply>(&reply, "remote reply").map_err(|e| message(&e))
            }
            .boxed()
        });
        serve_codemode_remote(
            request,
            exchange,
            CodemodeServeOptions {
                max_interrupt_budget: count(field(options, "maxInterruptBudget")),
                loop_iteration_limit: count(field(options, "loopIterationLimit")),
                max_memory_limit_bytes: count(field(options, "maxMemoryLimitBytes")),
                recursion_limit: count(field(options, "recursionLimit")).map(|n| n as usize),
                // The serving runtime's timezone (`process.env.TZ`).
                timezone: field(options, "timezone").as_string().map(|tz| tz.to_string_lossy()),
            },
        )
        .await;
        Ok(HostValue::Undefined)
    });
}
