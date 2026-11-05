//! Test-only host capabilities. Rust retains the tool task's execution decisions.
use super::*;
use base64::Engine;
use pi_chord::output::OutputLimits;
use pi_durable::harness::{declaration::ToolDeclaration, tool::*};
use std::sync::Weak;

#[derive(Default)]
pub(super) struct ToolBridge {
    pub(super) apis: Mutex<HashMap<u64, ToolExecutionApi>>,
    pub executions: AtomicU64,
    pub searches: AtomicU64,
    pub codemode_formats: AtomicU64,
    pub model_effects: AtomicU64,
    pub typed_requests: AtomicU64,
    pub catalog_queries: AtomicU64,
    definitions: Mutex<HashMap<u64, AnyTask>>,
    contents: Mutex<HashMap<u64, ToolContent>>,
    content_hosts: Mutex<HashMap<u64, u64>>,
    pub hook_calls: AtomicU64,
}
struct HostValue {
    hub: Weak<Hub>,
    id: u64,
}
impl HostedToolValue for HostValue {
    fn copy_json(&self) -> Result<JsonValue> {
        self.hub
            .upgrade()
            .ok_or_else(|| Error::message("Tool value host closed"))?
            .sync("tool.copyValue", encode(&self.id))
    }
}
pub(super) struct HostTool {
    pub hub: Weak<Hub>,
    pub id: u64,
    pub declaration: ToolDeclaration,
    pub replay: ReplayPolicy,
    pub limits: OutputLimits,
    pub schema: Option<JsonValue>,
    pub execution_mode: Option<ToolExecutionMode>,
}
impl ExecutableTool for HostTool {
    fn declaration(&self) -> ToolDeclaration {
        self.declaration.clone()
    }
    fn replay(&self) -> ReplayPolicy {
        self.replay
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.execution_mode
    }
    fn output_limits(&self) -> OutputLimits {
        self.limits
    }
    fn structured_output_schema(&self) -> Option<JsonValue> {
        self.schema.clone()
    }
    fn prepare_arguments(&self, args: JsonValue) -> Result<JsonValue> {
        self.hub
            .upgrade()
            .ok_or_else(|| Error::message("Host closed"))?
            .sync(
                "tool.prepare",
                object([("tool", encode(&self.id)), ("arguments", args)]),
            )
    }
    fn validate_arguments(&self, call: &ToolCall, args: JsonValue) -> Result<JsonObject> {
        decode(
            &self
                .hub
                .upgrade()
                .ok_or_else(|| Error::message("Host closed"))?
                .sync(
                    "tool.validate",
                    object([
                        ("tool", encode(&self.id)),
                        ("call", encode(call)),
                        ("arguments", args),
                    ]),
                )?,
        )
    }
    fn execute(
        &self,
        args: JsonObject,
        api: ToolExecutionApi,
        context: Context,
    ) -> StorageFuture<ToolExecutionResult> {
        let Some(hub) = self.hub.upgrade() else {
            return Box::pin(async { Err(Error::message("Host closed")) });
        };
        hub.tool.executions.fetch_add(1, Ordering::Relaxed);
        let id = hub.next.fetch_add(1, Ordering::Relaxed);
        let runtime = hub.bind_tool_runtime(&api.erased(), &context);
        hub.tool.apis.lock().insert(id, api.clone());
        let operation = hub.call_context(
            "tool.execute",
            object([
                ("tool", encode(&self.id)),
                ("arguments", encode(&args)),
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
                    encode(&api.env.as_ref().and_then(|env| hub.environment.handle(env))),
                ),
                (
                    "outputWindow",
                    api.output_window
                        .as_ref()
                        .map(|window| {
                            object([
                                ("maxBytes", encode(&window.max_bytes)),
                                ("maxLines", encode(&window.max_lines)),
                                ("minIntervalMs", encode(&window.min_interval_ms)),
                                ("bytesPerSecond", encode(&window.bytes_per_second)),
                            ])
                        })
                        .unwrap_or(JsonValue::Null),
                ),
            ]),
            &context,
        );
        Box::pin(async move {
            hub.tool_result_from_host(
                &operation
                    .await?
                    .ok_or_else(|| Error::message("Tool returned undefined"))?,
            )
        })
    }
}
pub(super) struct HostToolHooks {
    pub hub: Weak<Hub>,
    pub id: u64,
}
impl ToolHookProvider for HostToolHooks {
    fn before_tool(&self) -> Result<Option<BeforeTool>> {
        let hub = self
            .hub
            .upgrade()
            .ok_or_else(|| Error::message("Host closed"))?;
        let callback: Option<u64> = decode(&hub.sync(
            "hookMember",
            object([
                ("handlers", encode(&self.id)),
                ("name", encode(&"beforeTool")),
            ]),
        )?)?;
        Ok(callback.map(|callback| {
            Arc::new(
                move |call: ToolHookCall,
                      runtime: ToolRuntime,
                      context: Context|
                      -> StorageFuture<Option<ToolDecision>> {
                    hub.tool.hook_calls.fetch_add(1, Ordering::Relaxed);
                    let operation = hub.call_context(
                        "tool.beforeHook",
                        hub.tool_hook_request(callback, call, &runtime, &context),
                        &context,
                    );
                    Box::pin(async move {
                        operation
                            .await?
                            .map(|value| {
                                Ok::<ToolDecision, Error>(ToolDecision {
                                    arguments: value
                                        .as_object()
                                        .and_then(|object| {
                                            object.get(&JsonString::from("arguments"))
                                        })
                                        .cloned(),
                                    block: if value["block"].is_null() {
                                        None
                                    } else {
                                        Some(field(&value, "block")?)
                                    },
                                })
                            })
                            .transpose()
                    })
                },
            ) as BeforeTool
        }))
    }
    fn after_tool(&self) -> Result<Option<AfterTool>> {
        let hub = self
            .hub
            .upgrade()
            .ok_or_else(|| Error::message("Host closed"))?;
        let callback: Option<u64> = decode(&hub.sync(
            "hookMember",
            object([
                ("handlers", encode(&self.id)),
                ("name", encode(&"afterTool")),
            ]),
        )?)?;
        Ok(callback.map(|callback| {
            Arc::new(
                move |call: ToolHookCall,
                      result: ToolExecutionResult,
                      runtime: ToolRuntime,
                      context: Context|
                      -> StorageFuture<Option<ToolExecutionResult>> {
                    hub.tool.hook_calls.fetch_add(1, Ordering::Relaxed);
                    let mut request = hub.tool_hook_request(callback, call, &runtime, &context);
                    request
                        .as_object_mut()
                        .unwrap()
                        .insert("result".into(), hub.tool_result_to_host(&result));
                    let operation = hub.call_context("tool.afterHook", request, &context);
                    let hub = hub.clone();
                    Box::pin(async move {
                        operation
                            .await?
                            .map(|value| hub.tool_result_from_host(&value))
                            .transpose()
                    })
                },
            ) as AfterTool
        }))
    }
}
impl Hub {
    fn tool_hook_request(
        self: &Arc<Self>,
        callback: u64,
        call: ToolHookCall,
        runtime: &ToolRuntime,
        context: &Context,
    ) -> JsonValue {
        let mut value = encode(&call.call);
        if let Some(parent) = call.parent {
            value.as_object_mut().unwrap().insert(
                "parent".into(),
                object([
                    ("taskId", encode(&parent.task_id)),
                    ("callId", encode(&parent.call_id)),
                ]),
            );
        }
        object([
            ("handler", encode(&callback)),
            ("call", value),
            (
                "runtime",
                encode(&self.bind_tool_runtime(&runtime.erased(), context)),
            ),
            ("taskId", encode(&runtime.task_id())),
            ("conversationId", encode(&runtime.conversation_id())),
            (
                "harness",
                encode(&context.value(&HARNESS_CONTEXT).map(|id| *id)),
            ),
        ])
    }
    fn tool_result_to_host(&self, result: &ToolExecutionResult) -> JsonValue {
        let mut plain = result.clone();
        let mut values = object([]);
        if let Some(value) = result
            .details
            .as_ref()
            .and_then(ToolValue::hosted::<HostValue>)
        {
            values
                .as_object_mut()
                .unwrap()
                .insert("details".into(), encode(&value.id));
            plain.details = None;
        }
        if let Some(value) = result
            .structured_output
            .as_ref()
            .and_then(ToolValue::hosted::<HostValue>)
        {
            values
                .as_object_mut()
                .unwrap()
                .insert("structuredOutput".into(), encode(&value.id));
            plain.structured_output = None;
        }
        let mut value = object([("result", encode(&plain)), ("values", values)]);
        if let Some(output) = &result.output {
            let mut contents = self.tool.contents.lock();
            let id = contents
                .iter()
                .find(|(_, held)| Arc::ptr_eq(held, output))
                .map(|(id, _)| *id)
                .unwrap_or_else(|| self.next.fetch_add(1, Ordering::Relaxed));
            contents.insert(id, output.clone());
            let fields = value.as_object_mut().unwrap();
            fields.insert("contentRef".into(), encode(&id));
            if let Some(host) = self.tool.content_hosts.lock().get(&id) {
                fields.insert("contentHost".into(), encode(host));
            }
        }
        value
    }
    fn tool_result_from_host(self: &Arc<Self>, reply: &JsonValue) -> Result<ToolExecutionResult> {
        let mut result: ToolExecutionResult = field(reply, "result")?;
        if let Some(id) = reply["values"]["details"].as_u64() {
            result.details = Some(ToolValue::Hosted(Arc::new(HostValue {
                hub: Arc::downgrade(self),
                id,
            })));
        }
        if let Some(id) = reply["values"]["structuredOutput"].as_u64() {
            result.structured_output = Some(ToolValue::Hosted(Arc::new(HostValue {
                hub: Arc::downgrade(self),
                id,
            })));
        }
        if let Some(output) = result.output.as_mut() {
            let mut contents = self.tool.contents.lock();
            let host = reply["contentHost"].as_u64();
            let id = reply["contentRef"].as_u64().or_else(|| {
                host.and_then(|host| {
                    self.tool
                        .content_hosts
                        .lock()
                        .iter()
                        .find(|(_, held)| **held == host)
                        .map(|(id, _)| *id)
                })
            });
            if let Some(prior) = id.and_then(|id| contents.get(&id))
                && **prior == **output
            {
                *output = prior.clone();
            }
            let id = contents
                .iter()
                .find(|(_, prior)| Arc::ptr_eq(prior, output))
                .map(|(id, _)| *id)
                .unwrap_or_else(|| self.next.fetch_add(1, Ordering::Relaxed));
            contents.insert(id, output.clone());
            if let Some(host) = host {
                self.tool.content_hosts.lock().insert(id, host);
            }
        }
        Ok(result)
    }

    pub(super) fn bind_tool_runtime(
        self: &Arc<Self>,
        runtime: &ErasedRuntime,
        context: &Context,
    ) -> u64 {
        if let Some(id) = self
            .native_runtimes
            .lock()
            .iter()
            .find(|(_, held)| held.same_invocation(runtime))
            .map(|(id, _)| *id)
        {
            return id;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.native_runtimes.lock().insert(id, runtime.clone());
        self.forward_runtime_signal(id, runtime.signal(), context);
        id
    }
    pub(super) fn stock_catalog_task(self: &Arc<Self>, description: &JsonValue) -> Result<AnyTask> {
        if description["nativeCompaction"].as_bool() == Some(true) {
            let task = pi_durable::harness::compaction::compaction_task().erased();
            self.tool
                .definitions
                .lock()
                .insert(field(description, "id")?, task.clone());
            return Ok(task);
        }
        if description["nativeGeneration"].as_bool() == Some(true) {
            let id = field(description, "id")?;
            if let Some(task) = self.tool.definitions.lock().get(&id).cloned() {
                return Ok(task);
            }
            let task = pi_durable::harness::generation::generation_task::<
                registry_bridge::ToolHandle,
                registry_bridge::SectionHandle,
            >()
            .erased();
            self.tool.definitions.lock().insert(id, task.clone());
            return Ok(task);
        }
        if description["nativeTool"].as_bool() == Some(true) {
            let task = TOOL_TASK.erased();
            self.tool
                .definitions
                .lock()
                .insert(field(description, "id")?, task.clone());
            return Ok(task);
        }
        Ok(self.definition(description)?.erased())
    }
    pub(super) fn bound_task_id(&self, task: &AnyTask) -> Result<u64> {
        if let Some(id) = self
            .tool
            .definitions
            .lock()
            .iter()
            .find(|(_, held)| held.same_definition(task))
            .map(|(id, _)| *id)
        {
            return Ok(id);
        }
        self.tasks
            .lock()
            .iter()
            .find(|(_, held)| held.erased().same_definition(task))
            .map(|(id, _)| *id)
            .ok_or_else(|| Error::message("Task has no original host definition"))
    }
    pub(super) fn host_tool(
        self: &Arc<Self>,
        value: &JsonValue,
    ) -> Result<Arc<registry_bridge::ToolHandle>> {
        let mut tool: registry_bridge::ToolHandle = decode(value)?;
        if value["nativeBuiltin"].string_equals("codemode") {
            tool.executable = Some(Arc::new(self.native_codemode(&value["codemodeOptions"])?));
        } else if value["nativeBuiltin"].string_equals("tool_search") {
            let mut native = pi_durable::tool_search::create_tool_search_tool();
            let execute = native.execute.clone();
            let hub = Arc::downgrade(self);
            native.execute = Arc::new(move |args, api, context| {
                if let Some(hub) = hub.upgrade() {
                    hub.tool.searches.fetch_add(1, Ordering::Relaxed);
                }
                execute(args, api, context)
            });
            tool.executable = Some(Arc::new(native));
        } else if let Some(name) = value["nativeBuiltin"].to_utf8().filter(|name| {
            [
                pi_durable::codemode::models::CLASSIFY_TOOL,
                pi_durable::codemode::models::GENERATE_IMAGES_TOOL,
            ]
            .contains(&name.as_str())
        }) {
            let mut native = pi_durable::codemode::models::create_model_tools()
                .into_iter()
                .find(|tool| tool.declaration.name.to_string_lossy() == name)
                .unwrap();
            let execute = native.execute.clone();
            let hub = Arc::downgrade(self);
            native.execute = Arc::new(move |args, api, context| {
                if let Some(hub) = hub.upgrade() {
                    hub.tool.model_effects.fetch_add(1, Ordering::Relaxed);
                }
                execute(args, api, context)
            });
            tool.executable = Some(Arc::new(native));
        } else if value["declaration"].is_object() {
            tool.executable = Some(Arc::new(HostTool {
                hub: Arc::downgrade(self),
                id: field(value, "id")?,
                declaration: field(value, "declaration")?,
                replay: if value["replay"].string_equals("safe") {
                    ReplayPolicy::Safe
                } else {
                    ReplayPolicy::Unsafe
                },
                limits: field(value, "limits")?,
                execution_mode: field(value, "executionMode")?,
                schema: value
                    .as_object()
                    .unwrap()
                    .get(&JsonString::from("schema"))
                    .cloned(),
            }));
        }
        Ok(Arc::new(tool))
    }
    pub(super) fn tool_request(
        self: &Arc<Self>,
        method: &str,
        request: JsonValue,
        context: Context,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        let api = self
            .tool
            .apis
            .lock()
            .get(&field::<u64>(&request, "api")?)
            .cloned()
            .ok_or_else(|| Error::message("Unknown tool API"))?;
        match method {
            "tool.commit" => {
                let request_id = field(&request, "id")?;
                let callback = field(&request, "callback")?;
                let hub = self.clone();
                Ok(api.commit(
                    move |tx| async move { hub.callback(callback, tx, None, request_id).await },
                    &context,
                ))
            }
            "tool.createTask" => {
                let options = &request["options"];
                let ownership = task_ownership(options)?;
                let options = ToolTaskOptions {
                    ownership,
                    trace: if options["trace"].string_equals("root") {
                        Some(TaskTrace::Root)
                    } else if options["trace"].is_object() {
                        Some(TaskTrace::Carrier(field(options, "trace")?))
                    } else {
                        None
                    },
                    links: field(options, "links")?,
                    background: options["background"].as_bool().unwrap_or(false),
                    abandon_on_restart: options["abandonOnRestart"].as_bool(),
                    submission_ids: field(options, "submissionIds")?,
                };
                let task = self.stock_catalog_task(&request["definition"])?;
                let operation =
                    api.create_erased_task(&task, request["input"].clone(), options, &context);
                Ok(Box::pin(async move { Ok(Some(encode(&operation.await?))) }))
            }
            "tool.output" => {
                let skipped = if request["skipped"].is_object() {
                    Some(field(&request, "skipped")?)
                } else {
                    None
                };
                if let Some(bytes) = request["bytes"].to_utf8() {
                    api.output_bytes(
                        &base64::engine::general_purpose::STANDARD
                            .decode(bytes)
                            .map_err(|error| Error::message(error.to_string()))?,
                        skipped,
                    )?;
                } else {
                    api.output(field::<JsonString>(&request, "text")?, skipped)?;
                }
                Ok(Box::pin(async { Ok(None) }))
            }
            "tool.diagnostic" => {
                api.diagnostic(field(&request, "diagnostic")?)?;
                Ok(Box::pin(async { Ok(None) }))
            }
            "tool.details" => {
                let operation = api.details(request["value"].clone(), &context);
                Ok(Box::pin(async move {
                    operation.await?;
                    Ok(None)
                }))
            }
            "tool.retained" => {
                let value = api.retained_output()?;
                Ok(Box::pin(async move {
                    Ok(Some(object([
                        ("text", encode(&value.text)),
                        ("truncated", encode(&value.truncated)),
                    ])))
                }))
            }
            "tool.nested" => {
                let operation = api.execute_tool(
                    field::<JsonString>(&request, "name")?,
                    field(&request, "arguments")?,
                    &context,
                    NestedCallOptions {
                        key: request["options"]["key"].as_string().cloned(),
                        progress: request["options"]["progress"].as_bool(),
                    },
                );
                Ok(Box::pin(async move { Ok(Some(encode(&operation.await?))) }))
            }
            _ => Err(Error::message(format!(
                "Unknown native tool method {method}"
            ))),
        }
    }
}
