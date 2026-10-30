//! The stock VM remains an opaque capability; replay decisions stay in Rust.
use super::*;
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use pi_durable::codemode::{self, describe::*, vm::*, *};
use pi_durable::harness::tool::NativeTool;
use std::sync::Weak;
#[derive(Default)]
pub(super) struct CodemodeBridge {
    callbacks: Mutex<HashMap<u64, Callbacks>>,
    pub executions: AtomicU64,
    pub starts: AtomicU64,
    pub deliveries: AtomicU64,
    pub globals: AtomicU64,
    pub checkpoint_policies: AtomicU64,
}
#[derive(Clone)]
struct Callbacks {
    started: Arc<dyn Fn(JsonString) + Send + Sync>,
    call: Arc<dyn Fn(HostCall) + Send + Sync>,
    idle: Arc<dyn Fn(u64, u64) + Send + Sync>,
}
struct HostSandbox {
    hub: Weak<Hub>,
    id: u64,
}
struct HostRun {
    hub: Weak<Hub>,
    id: u64,
    result: Shared<BoxFuture<'static, Result<JsonValue>>>,
    watch: tokio::task::JoinHandle<()>,
}
impl Drop for HostRun {
    fn drop(&mut self) {
        self.watch.abort();
        if let Some(hub) = self.hub.upgrade() {
            hub.codemode.callbacks.lock().remove(&self.id);
            hub.emit(object([
                ("type", encode(&"codemode.vm.abort")),
                ("run", encode(&self.id)),
            ]));
            hub.emit(object([
                ("type", encode(&"codemode.vm.release")),
                ("run", encode(&self.id)),
            ]));
        }
    }
}
impl Sandbox for HostSandbox {
    fn start(&self, code: JsonString, options: RunOptions) -> Result<Arc<dyn Run>> {
        let hub = self
            .hub
            .upgrade()
            .ok_or_else(|| Error::message("VM host closed"))?;
        let id = hub.next.fetch_add(1, Ordering::Relaxed);
        hub.codemode.starts.fetch_add(1, Ordering::Relaxed);
        hub.codemode.callbacks.lock().insert(
            id,
            Callbacks {
                started: options.on_started,
                call: options.on_call,
                idle: options.on_idle,
            },
        );
        if let Err(error) = hub.sync(
            "codemode.vm.start",
            object([
                ("sandbox", encode(&self.id)),
                ("run", encode(&id)),
                ("code", encode(&code)),
                ("start", encode(&options.start)),
                ("timeoutMs", encode(&options.timeout_ms)),
                ("aborted", encode(&options.signal.aborted())),
            ]),
        ) {
            hub.codemode.callbacks.lock().remove(&id);
            return Err(error);
        }
        let pending = hub.call("codemode.vm.result", encode(&id));
        let result = async move {
            pending
                .await?
                .ok_or_else(|| Error::message("VM returned undefined"))
        }
        .boxed()
        .shared();
        let host = self.hub.clone();
        let signal = options.signal;
        let watch = tokio::spawn(async move {
            signal.cancelled().await;
            if let Some(hub) = host.upgrade() {
                hub.emit(object([
                    ("type", encode(&"codemode.vm.abort")),
                    ("run", encode(&id)),
                ]));
            }
        });
        Ok(Arc::new(HostRun {
            hub: self.hub.clone(),
            id,
            result,
            watch,
        }))
    }
}
impl Run for HostRun {
    fn finished(&self) -> bool {
        self.hub
            .upgrade()
            .and_then(|hub| hub.sync("codemode.vm.finished", encode(&self.id)).ok())
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }
    fn deliver(&self, seq: u64, at: f64, id: u64, outcome: Outcome) -> Result<()> {
        let hub = self
            .hub
            .upgrade()
            .ok_or_else(|| Error::message("VM host closed"))?;
        hub.codemode.deliveries.fetch_add(1, Ordering::Relaxed);
        hub.sync(
            "codemode.vm.deliver",
            object([
                ("run", encode(&self.id)),
                ("seq", encode(&seq)),
                ("at", encode(&at)),
                ("call", encode(&id)),
                ("outcome", outcome.to_json()),
            ]),
        )?;
        Ok(())
    }
    fn result(&self) -> StorageFuture<JsonValue> {
        Box::pin(self.result.clone())
    }
}
impl Hub {
    pub(super) fn codemode_event(&self, value: &JsonValue) -> Result<()> {
        let callbacks = self
            .codemode
            .callbacks
            .lock()
            .get(&field::<u64>(value, "run")?)
            .cloned();
        let Some(callbacks) = callbacks else {
            return Ok(());
        };
        match value["event"].to_utf8().as_deref() {
            Some("started") => (callbacks.started)(field(value, "fingerprint")?),
            Some("call") => (callbacks.call)(field(value, "call")?),
            Some("idle") => (callbacks.idle)(field(value, "seq")?, field(value, "issued")?),
            _ => return Err(Error::message("Unknown VM callback")),
        };
        Ok(())
    }
    pub(super) fn native_codemode(self: &Arc<Self>, options: &JsonValue) -> Result<NativeTool> {
        let mut config = CodemodeToolOptions::new(Arc::new(HostSandbox {
            hub: Arc::downgrade(self),
            id: field(options, "sandbox")?,
        }));
        config.models = options["models"].as_bool().unwrap_or(true);
        config.namespaces = field::<Option<_>>(options, "namespaces")?.unwrap_or_default();
        config.inline_budget = field::<Option<f64>>(options, "inlineBudget")?
            .unwrap_or(DEFAULT_CODEMODE_INLINE_BUDGET);
        config.globals = field::<Option<Vec<JsonValue>>>(options, "globals")?
            .unwrap_or_default()
            .into_iter()
            .map(|global| {
                let callback: u64 = field(&global, "callback")?;
                let hub = self.clone();
                Ok(CodemodeGlobal {
                    name: field(&global, "name")?,
                    spread: global["spread"].as_bool().unwrap_or(false),
                    execute: Arc::new(move |args, api, context| {
                        hub.codemode.globals.fetch_add(1, Ordering::Relaxed);
                        let runtime = hub.bind_tool_runtime(&api.erased(), &context);
                        let id = hub.next.fetch_add(1, Ordering::Relaxed);
                        hub.tool.apis.lock().insert(id, api.clone());
                        let descriptor = object([
                            ("api", encode(&id)),
                            ("runtime", encode(&runtime)),
                            ("callId", encode(&api.call_id)),
                            ("taskId", encode(&api.task_id())),
                            ("conversationId", encode(&api.conversation_id())),
                            (
                                "harness",
                                encode(&context.value(&HARNESS_CONTEXT).map(|id| *id)),
                            ),
                            (
                                "environment",
                                encode(
                                    &api.env.as_ref().and_then(|env| hub.environment.handle(env)),
                                ),
                            ),
                            ("outputWindow", JsonValue::Null),
                        ]);
                        hub.call_context(
                            "codemode.global",
                            object([
                                ("callback", encode(&callback)),
                                ("api", descriptor),
                                ("present", encode(&args.is_some())),
                                ("args", args.unwrap_or(JsonValue::Null)),
                            ]),
                            &context,
                        )
                    }),
                })
            })
            .collect::<Result<_>>()?;
        let mut tool = codemode::create_codemode_tool(config);
        let execute = tool.execute.clone();
        let hub = Arc::downgrade(self);
        tool.execute = Arc::new(move |args, api, context| {
            if let Some(hub) = hub.upgrade() {
                hub.codemode.executions.fetch_add(1, Ordering::Relaxed);
            }
            execute(args, api, context)
        });
        Ok(tool)
    }
    pub(super) fn codemode_metadata(
        self: &Arc<Self>,
        request: JsonValue,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        let name: String = field(&request, "method")?;
        let value = match name.as_str() {
            "codemode.declaration" => {
                encode(&self.native_codemode(&request["options"])?.declaration)
            }
            "codemode.description" => encode(&codemode_description(
                request["models"].as_bool().unwrap_or(true),
            )),
            "codemode.section" => {
                let tools = field::<Vec<JsonValue>>(&request, "tools")?
                    .iter()
                    .map(|tool| {
                        Ok(ToolMetadata {
                            declaration: field(tool, "declaration")?,
                            structured_output_schema: tool
                                .as_object()
                                .unwrap()
                                .get(&JsonString::from("schema"))
                                .cloned(),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let namespaces = field::<Vec<CodemodeNamespace>>(&request, "namespaces")?;
                let budget: f64 = field(&request, "budget")?;
                return Ok(Box::pin(async move {
                    Ok(codemode_section(&tools, &namespaces, budget)?.map(|value| encode(&value)))
                }));
            }
            "codemode.docCheckpoint" => {
                self.codemode
                    .checkpoint_policies
                    .fetch_add(1, Ordering::Relaxed);
                let definition = match request["kind"].to_utf8().as_deref() {
                    Some("pi.codemode.store") => CODEMODE_STORE_DOC.definition(),
                    Some("pi.codemode.run") => CODEMODE_RUN_DOC.definition(),
                    Some("pi.codemode.journal") => CODEMODE_JOURNAL_DOC.definition(),
                    _ => return Err(Error::message("Unknown code-mode document")),
                };
                encode(&definition.checkpoint_when(
                    &definition.initial(None)?,
                    &[],
                    CheckpointInfo {
                        deltas_since_base: field(&request, "deltas")?,
                    },
                )?)
            }
            _ => return Err(Error::message("Unknown code-mode metadata request")),
        };
        Ok(Box::pin(async { Ok(Some(value)) }))
    }
}
