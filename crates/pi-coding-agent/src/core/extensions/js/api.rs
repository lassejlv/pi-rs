//! Host-call dispatch and the `pi` object (ExtensionAPI) of JavaScript extensions.

use std::sync::Arc;

use futures::FutureExt;
use pi_jsrt::{JsHandle, JsRuntime};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{Bridge, CtxEntry, builtin, component, context, native, provider, sdk};
use crate::compat::AutocompleteItem;
use crate::core::bash_executor::BashResult;
use crate::core::exec::ExecOptions;
use crate::core::extensions::types::*;
use crate::core::mcp_servers::validate_mcp_server_config;
use crate::core::session_manager::CustomEntry;
use crate::core::tools::bash::{BashOperations, BashOperationsResult};

/// Parse `value` as `T`, treating JSON null as `None`.
pub(crate) fn opt<T: DeserializeOwned>(value: &Value) -> anyhow::Result<Option<T>> {
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_value(value.clone())?))
}

pub(crate) fn str_arg(payload: &Value, key: &str) -> String {
    payload.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

pub(crate) fn arg(payload: &Value, index: usize) -> Value {
    payload.get("a").and_then(|a| a.get(index)).cloned().unwrap_or(Value::Null)
}

/// Post a call to a JS function without waiting: re-entrant (synchronous, like an EventEmitter)
/// when already inside a host call on the JS thread, queued otherwise.
pub(crate) fn post_call(handle: &JsHandle, method: Option<&str>, args: Vec<Value>) {
    if handle.runtime().is_js_thread() {
        let _ = match method {
            Some(m) => handle.call_method_blocking(m, args),
            None => handle.call_blocking(args),
        };
        return;
    }
    // Polling once posts the request to the JS thread; dropping the future discards the result.
    let _ = match method {
        Some(m) => handle.call_method(m, args).boxed().now_or_never(),
        None => handle.call(args).boxed().now_or_never(),
    };
}

/// Synchronous host calls (`__piHost.call`).
pub(crate) fn dispatch(bridge: &Arc<Bridge>, rt: &JsRuntime, op: &str, payload: Value) -> anyhow::Result<Value> {
    match op {
        "unsub" => {
            if let Some(id) = payload.get("id").and_then(Value::as_u64)
                && let Some(unsubscribe) = bridge.unsubscribes.remove(id)
            {
                unsubscribe();
            }
            Ok(Value::Null)
        }
        "token.new" => Ok(json!(bridge.tokens.insert(CancellationToken::new()))),
        "token.cancel" => {
            if let Some(token) = bridge.token(&payload["id"]) {
                token.cancel();
            }
            Ok(Value::Null)
        }
        "cb" => {
            let id = payload["id"].as_u64().unwrap_or_default();
            let callback = bridge.callbacks.get(id).ok_or_else(|| anyhow::anyhow!("This callback is no longer valid"))?;
            let args = payload.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
            callback(args)
        }
        "version" => Ok(json!(crate::config::VERSION)),
        _ if op.starts_with("pi.") => pi_op(bridge, rt, op, payload),
        "ctx.get" | "ctx.release" | "ctx.call" | "sm" | "sm.static" | "mr" | "ui" => context::dispatch(bridge, op, payload),
        "theme" | "tui" | "kb" | "sv" | "overlay" | "footer" | "rc" | "rc.new" | "tui.util" | "tui.markdown"
        | "tui.markdownTheme" | "renderer.call" | "renderer.result" | "ac" => component::dispatch(bridge, op, payload),
        _ if op.starts_with("builtin.") || op.starts_with("ca") => builtin::dispatch(bridge, op, payload),
        "ai" => provider::dispatch(bridge, op, payload),
        "settings" | "settings.static" | "rt" | "rl" | "as" | "trust" | "pm" | "sdk.resolveCliModel" | "sdk.createExtensionRuntime"
        | "sdk.servicesPaths" | "sdk.extensionTool" => {
            sdk::dispatch(bridge, op, payload)
        }
        _ if op.starts_with("aip.") => native::dispatch(bridge, op, payload),
        _ => Err(anyhow::anyhow!("unknown host call {op}")),
    }
}

/// Asynchronous host calls (`__piHost.callAsync`).
pub(crate) async fn dispatch_async(bridge: Arc<Bridge>, _rt: JsRuntime, op: String, payload: Value) -> anyhow::Result<Value> {
    match op.as_str() {
        "pi.exec" => {
            let api = api_of(&bridge, &payload)?;
            let command = str_arg(&payload, "command");
            let args: Vec<String> = opt(&payload["args"])?.unwrap_or_default();
            let options = &payload["options"];
            let exec_options = ExecOptions {
                signal: bridge.token(&options["signal"]),
                timeout: options["timeout"].as_f64().map(|t| t as i64),
                cwd: options["cwd"].as_str().map(str::to_string),
            };
            let result = api.exec(&command, &args, exec_options).await?;
            Ok(serde_json::to_value(result)?)
        }
        "pi.setModel" => {
            let api = api_of(&bridge, &payload)?;
            let model = serde_json::from_value(payload["model"].clone())?;
            Ok(json!(api.set_model(model).await?))
        }
        "cb.async" => {
            let id = payload["id"].as_u64().unwrap_or_default();
            let callback =
                bridge.async_callbacks.get(id).ok_or_else(|| anyhow::anyhow!("This callback is no longer valid"))?;
            let args = payload.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
            callback(args).await
        }
        "ctx.async" | "mr.async" | "ui.async" | "mr.stream" | "sm.list" => context::dispatch_async(bridge, &op, payload).await,
        "ac.async" => component::dispatch_async(bridge, &op, payload).await,
        "tui.clipboard" => component::native_clipboard_call(&payload).await,
        "builtin.execute" | "ca.localBashExec" | "ca.copyToClipboard" | "ca.image" | "ca.compaction" => builtin::dispatch_async(bridge, &op, payload).await,
        "ai.stream" | "stream.next" => provider::dispatch_async(bridge, &op, payload).await,
        "settings.async" | "rt.async" | "rt.stream" | "rl.async" | "as.async" | "builtin.ext.run" | "pm.async" | "sdk.resolveModelScope"
        | "sdk.discoverAndLoadExtensions" | "sdk.finishServices" => {
            sdk::dispatch_async(bridge, &op, payload).await
        }
        _ if op.starts_with("aip.") => native::dispatch_async(bridge, &op, payload).await,
        _ => Err(anyhow::anyhow!("unknown async host call {op}")),
    }
}

fn api_of(bridge: &Bridge, payload: &Value) -> anyhow::Result<ExtensionAPIRef> {
    let id = payload["api"].as_u64().unwrap_or_default();
    bridge.apis.get(id).ok_or_else(|| anyhow::anyhow!("unknown extension API {id}"))
}

fn handle_of(bridge: &Bridge, value: &Value, what: &str) -> anyhow::Result<JsHandle> {
    bridge.handle(value).ok_or_else(|| anyhow::anyhow!("{what} must be a function"))
}

fn pi_op(bridge: &Arc<Bridge>, _rt: &JsRuntime, op: &str, payload: Value) -> anyhow::Result<Value> {
    let api = api_of(bridge, &payload)?;
    match op {
        "pi.path" => Ok(json!(api.extension_path().unwrap_or_default())),
        "pi.on" => {
            let event = str_arg(&payload, "event");
            let handler = handle_of(bridge, &payload["handler"], "handler")?;
            let unsubscribe = if event == "project_trust" {
                api.on_project_trust(js_project_trust_handler(bridge.clone(), handler))?
            } else {
                api.on(&event, js_event_handler(bridge.clone(), handler))?
            };
            Ok(json!(bridge.unsubscribes.insert(unsubscribe)))
        }
        "pi.registerTool" => {
            let tool = js_tool_definition(bridge, &payload["tool"])?;
            api.register_tool(tool)?;
            Ok(Value::Null)
        }
        "pi.registerCommand" => {
            let name = str_arg(&payload, "name");
            let options = &payload["options"];
            let handler = bridge.handle(&options["handler"]);
            let Some(handler) = handler else {
                // Rust validates the name first, with the TS message.
                api.register_command(
                    &name,
                    CommandOptions { description: None, get_argument_completions: None, handler: Arc::new(|_, _| Box::pin(async { Ok(()) })) },
                )?;
                anyhow::bail!(
                    "Command \"/{name}\" registered by extension \"{}\" must define handler().",
                    api.extension_path().unwrap_or_default()
                );
            };
            let completions = bridge.handle(&options["getArgumentCompletions"]).map(|h| js_completions(bridge.clone(), h));
            api.register_command(
                &name,
                CommandOptions {
                    description: options["description"].as_str().map(str::to_string),
                    get_argument_completions: completions,
                    handler: js_command_handler(bridge.clone(), handler),
                },
            )?;
            Ok(Value::Null)
        }
        "pi.registerShortcut" => {
            let shortcut = str_arg(&payload, "shortcut");
            let options = &payload["options"];
            let handler = handle_of(bridge, &options["handler"], "shortcut handler")?;
            let b = bridge.clone();
            api.register_shortcut(
                &shortcut,
                ShortcutOptions {
                    description: options["description"].as_str().map(str::to_string),
                    handler: Arc::new(move |ctx| {
                        let b = b.clone();
                        let handler = handler.clone();
                        Box::pin(async move {
                            let ctx_id = b.register_ctx(CtxEntry::Base(ctx));
                            b.glue_call("invokeShortcut", vec![handler.to_value(), json!(ctx_id)]).await?;
                            Ok(())
                        })
                    }),
                },
            )?;
            Ok(Value::Null)
        }
        "pi.registerFlag" => {
            let name = str_arg(&payload, "name");
            let options = &payload["options"];
            // TS only checks `typeof default === type`; a flag without a known type parses as
            // a string option on the command line.
            let declared = options["type"].as_str().unwrap_or("undefined");
            let flag_type = if declared == "boolean" { FlagType::Boolean } else { FlagType::String };
            let got = match &options["default"] {
                Value::Null => None,
                Value::Bool(_) => Some("boolean"),
                Value::String(_) => Some("string"),
                Value::Number(_) => Some("number"),
                Value::Array(_) | Value::Object(_) => Some("object"),
            };
            if let Some(got) = got
                && got != declared
            {
                anyhow::bail!("Invalid default for flag \"{name}\": expected {declared}, got {got}");
            }
            let default = match &options["default"] {
                Value::Bool(b) => Some(FlagValue::Bool(*b)),
                Value::String(s) => Some(FlagValue::String(s.clone())),
                _ => None,
            };
            api.register_flag(
                &name,
                FlagOptions { description: options["description"].as_str().map(str::to_string), flag_type, default },
            )?;
            Ok(Value::Null)
        }
        "pi.getFlag" => Ok(serde_json::to_value(api.get_flag(&str_arg(&payload, "name"))?)?),
        "pi.registerMessageRenderer" => {
            let custom_type = str_arg(&payload, "customType");
            let renderer = handle_of(bridge, &payload["renderer"], "renderer")?;
            api.register_message_renderer(&custom_type, component::js_message_renderer(bridge.clone(), renderer))?;
            Ok(Value::Null)
        }
        "pi.registerEntryRenderer" => {
            let custom_type = str_arg(&payload, "customType");
            let renderer = handle_of(bridge, &payload["renderer"], "renderer")?;
            api.register_entry_renderer(&custom_type, component::js_entry_renderer(bridge.clone(), renderer))?;
            Ok(Value::Null)
        }
        "pi.registerMarkdownTransformer" => {
            let transformer = handle_of(bridge, &payload["transformer"], "transformer")?;
            api.register_markdown_transformer(Arc::new(move |markdown: &str, ctx: &MarkdownTransformContext| {
                match transformer.call_blocking(vec![json!(markdown), serde_json::to_value(ctx).unwrap_or(Value::Null)]) {
                    Ok(Value::String(s)) => s,
                    _ => markdown.to_string(),
                }
            }))?;
            Ok(Value::Null)
        }
        "pi.registerToolRenderer" => {
            let resolver = handle_of(bridge, &payload["resolver"], "resolver")?;
            api.register_tool_renderer(component::js_tool_renderer_resolver(bridge.clone(), resolver))?;
            Ok(Value::Null)
        }
        "pi.sendMessage" => {
            let message: CustomMessageInput = serde_json::from_value(payload["message"].clone())?;
            let options: SendMessageOptions = opt(&payload["options"])?.unwrap_or_default();
            api.send_message(message, options)?;
            Ok(Value::Null)
        }
        "pi.sendUserMessage" => {
            let content = serde_json::from_value(payload["content"].clone())?;
            let options: SendUserMessageOptions = opt(&payload["options"])?.unwrap_or_default();
            api.send_user_message(content, options)?;
            Ok(Value::Null)
        }
        "pi.appendEntry" => {
            let data = if payload["data"].is_null() { None } else { Some(payload["data"].clone()) };
            api.append_entry(&str_arg(&payload, "customType"), data)?;
            Ok(Value::Null)
        }
        "pi.setSessionName" => {
            api.set_session_name(&str_arg(&payload, "name"))?;
            Ok(Value::Null)
        }
        "pi.getSessionName" => Ok(json!(api.get_session_name()?)),
        "pi.setLabel" => {
            api.set_label(&str_arg(&payload, "entryId"), payload["label"].as_str())?;
            Ok(Value::Null)
        }
        "pi.getActiveTools" => Ok(json!(api.get_active_tools()?)),
        "pi.getAllTools" => Ok(Value::Array(api.get_all_tools()?.iter().map(tool_info_json).collect())),
        "pi.getSettings" => Ok(serde_json::to_value(api.get_settings()?)?),
        "pi.setActiveTools" => {
            let names: Vec<String> = opt(&payload["toolNames"])?.unwrap_or_default();
            api.set_active_tools(names)?;
            Ok(Value::Null)
        }
        "pi.getCommands" => Ok(serde_json::to_value(api.get_commands()?)?),
        "pi.getThinkingLevel" => Ok(serde_json::to_value(api.get_thinking_level()?)?),
        "pi.setThinkingLevel" => {
            api.set_thinking_level(serde_json::from_value(payload["level"].clone())?)?;
            Ok(Value::Null)
        }
        "pi.registerProvider" => {
            let name = str_arg(&payload, "name");
            let config = provider::js_provider_config(bridge, &payload["config"])?;
            api.register_provider(ProviderRegistration::Config { name, config })?;
            Ok(Value::Null)
        }
        "pi.registerNativeProvider" => {
            let provider = native::native_provider(bridge, &payload["provider"])?;
            api.register_provider(ProviderRegistration::Native(provider))?;
            Ok(Value::Null)
        }
        "pi.unregisterProvider" => {
            api.unregister_provider(&str_arg(&payload, "name"))?;
            Ok(Value::Null)
        }
        "pi.registerMcpServer" => {
            let name = str_arg(&payload, "name");
            let config = validate_mcp_server_config(&name, payload["config"].clone()).into_result().map_err(|e| {
                anyhow::anyhow!(
                    "Invalid MCP server registered by extension \"{}\": {e}",
                    api.extension_path().unwrap_or_default()
                )
            })?;
            api.register_mcp_server(&name, config)?;
            Ok(Value::Null)
        }
        "pi.unregisterMcpServer" => {
            api.unregister_mcp_server(&str_arg(&payload, "name"))?;
            Ok(Value::Null)
        }
        "pi.getMcpServers" => Ok(serde_json::to_value(api.get_mcp_servers()?)?),
        "pi.registerVirtualModel" => {
            let model = provider::js_virtual_model(bridge, &payload["model"])?;
            api.register_virtual_model(model)?;
            Ok(Value::Null)
        }
        "pi.unregisterVirtualModel" => {
            api.unregister_virtual_model(&str_arg(&payload, "provider"), &str_arg(&payload, "id"))?;
            Ok(Value::Null)
        }
        "pi.events.emit" => {
            api.events().emit(&str_arg(&payload, "channel"), payload["data"].clone());
            Ok(Value::Null)
        }
        "pi.events.on" => {
            let handler = handle_of(bridge, &payload["handler"], "event handler")?;
            let b = bridge.clone();
            let unsubscribe = api.events().on(
                &str_arg(&payload, "channel"),
                Arc::new(move |data| {
                    post_call(&b.glue, Some("invokeEventBus"), vec![handler.to_value(), data]);
                }),
            );
            Ok(json!(bridge.unsubscribes.insert(unsubscribe)))
        }
        _ => Err(anyhow::anyhow!("unknown host call {op}")),
    }
}

pub(crate) fn tool_info_json(info: &ToolInfo) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("name".into(), json!(info.name));
    out.insert("description".into(), json!(info.description));
    out.insert("parameters".into(), info.parameters.clone());
    if let Some(g) = &info.prompt_guidelines {
        out.insert("promptGuidelines".into(), json!(g));
    }
    out.insert("exposure".into(), json!(info.exposure.as_str()));
    if let Some(ns) = &info.namespace {
        out.insert("namespace".into(), serde_json::to_value(ns).unwrap_or(Value::Null));
    }
    if let Some(a) = &info.annotations {
        out.insert("annotations".into(), serde_json::to_value(a).unwrap_or(Value::Null));
    }
    out.insert("sourceInfo".into(), serde_json::to_value(&info.source_info).unwrap_or(Value::Null));
    Value::Object(out)
}

/// A JS event handler as an [`ExtensionHandler`]. The event is passed as JSON; in-place
/// mutations are read back by `invokeHandler` (bridge.js) and returned as replacements.
pub(crate) fn js_event_handler(bridge: Arc<Bridge>, handler: JsHandle) -> ExtensionHandler {
    Arc::new(move |event: ExtensionEvent, ctx: ExtensionContext| {
        let bridge = bridge.clone();
        let handler = handler.clone();
        Box::pin(async move {
            let signal = match &event {
                ExtensionEvent::SessionBeforeCompact(e) => Some(e.signal.clone()),
                ExtensionEvent::SessionBeforeTree(e) => Some(e.signal.clone()),
                _ => None,
            };
            let is_user_bash = matches!(event, ExtensionEvent::UserBash(_));
            let transient = matches!(event, ExtensionEvent::BeforeAgentStart(_));
            let value = serde_json::to_value(&event)?;
            let ctx_id = if transient { bridge.contexts.insert(CtxEntry::Base(ctx)) } else { bridge.register_ctx(CtxEntry::Base(ctx)) };
            let (signal_value, _guard) = match &signal {
                Some(token) => {
                    let (v, g) = bridge.signal_for(token);
                    (v, Some(g))
                }
                None => (Value::Null, None),
            };
            let result = bridge
                .glue_call("invokeHandler", vec![handler.to_value(), value, json!(ctx_id), json!("base"), signal_value])
                .await;
            if transient {
                bridge.detach_ctx(ctx_id, "base");
            }
            let result = result?;
            if result.is_null() {
                return Ok(None);
            }
            if is_user_bash {
                return user_bash_result(&bridge, result).map(Some);
            }
            Ok(Some(ExtensionEventResult::Json(result)))
        })
    })
}

fn user_bash_result(bridge: &Arc<Bridge>, result: Value) -> anyhow::Result<ExtensionEventResult> {
    if let Some(r) = result.get("result") {
        let parsed: Result<BashResult, _> = serde_json::from_value(r.clone());
        return Ok(match parsed {
            Ok(r) => ExtensionEventResult::UserBash(UserBashEventResult::Result(r)),
            Err(_) => ExtensionEventResult::Json(json!({ "$invalid": true })),
        });
    }
    if let Some(exec) = result.get("operations").and_then(|o| bridge.handle(&o["exec"])) {
        return Ok(ExtensionEventResult::UserBash(UserBashEventResult::Operations(js_bash_operations(bridge.clone(), exec))));
    }
    Ok(ExtensionEventResult::Json(json!({ "$invalid": true })))
}

/// `BashOperations` whose `exec` is a JS function `(command, cwd, { onData, signal, timeout, env })`.
pub(crate) fn js_bash_operations(bridge: Arc<Bridge>, exec: JsHandle) -> BashOperations {
    BashOperations {
        exec: Arc::new(move |command, cwd, options| {
            let bridge = bridge.clone();
            let exec = exec.clone();
            Box::pin(async move {
                let on_data = options.on_data.clone();
                let cb = bridge.register_callback(Arc::new(move |args| {
                    let bytes = super::provider::bytes_of(args.first().unwrap_or(&Value::Null));
                    on_data(bytes);
                    Ok(Value::Null)
                }));
                let (signal, _guard) = match &options.signal {
                    Some(token) => {
                        let (v, g) = bridge.signal_for(token);
                        (v, Some(g))
                    }
                    None => (Value::Null, None),
                };
                let result = bridge
                    .glue_call(
                        "invokeBashExec",
                        vec![
                            exec.to_value(),
                            json!(command),
                            json!(cwd),
                            json!(cb),
                            signal,
                            json!(options.timeout),
                            serde_json::to_value(&options.env).unwrap_or(Value::Null),
                        ],
                    )
                    .await;
                bridge.callbacks.remove(cb);
                let result = result?;
                Ok(serde_json::from_value::<BashOperationsResult>(result).unwrap_or(BashOperationsResult { exit_code: None }))
            })
        }),
    }
}

fn js_project_trust_handler(bridge: Arc<Bridge>, handler: JsHandle) -> ProjectTrustHandler {
    Arc::new(move |event, ctx| {
        let bridge = bridge.clone();
        let handler = handler.clone();
        Box::pin(async move {
            let ctx_id = bridge.contexts.insert(CtxEntry::Trust(ctx));
            let result = bridge
                .glue_call("invokeProjectTrust", vec![handler.to_value(), serde_json::to_value(&event)?, json!(ctx_id)])
                .await;
            bridge.detach_ctx(ctx_id, "trust");
            let result = result?;
            Ok(serde_json::from_value(result)?)
        })
    })
}

fn js_command_handler(bridge: Arc<Bridge>, handler: JsHandle) -> CommandHandler {
    Arc::new(move |args, ctx| {
        let bridge = bridge.clone();
        let handler = handler.clone();
        Box::pin(async move {
            let ctx_id = bridge.register_ctx(CtxEntry::Command(ctx));
            let result =
                bridge.glue_call("invokeCommand", vec![handler.to_value(), json!(args), json!(ctx_id), json!("command")]).await;
            bridge.detach_ctx(ctx_id, "command");
            result.map(|_| ())
        })
    })
}

fn js_completions(bridge: Arc<Bridge>, handler: JsHandle) -> ArgumentCompletionsFn {
    Arc::new(move |prefix| {
        let bridge = bridge.clone();
        let handler = handler.clone();
        Box::pin(async move {
            let value = bridge.glue_call("callFunctionAsync", vec![handler.to_value(), json!([prefix])]).await.ok()?;
            let items = value.as_array()?;
            Some(items.iter().map(autocomplete_item).collect())
        })
    })
}

pub(crate) fn autocomplete_item(value: &Value) -> AutocompleteItem {
    AutocompleteItem {
        value: value["value"].as_str().unwrap_or_default().to_string(),
        label: value["label"].as_str().or(value["value"].as_str()).unwrap_or_default().to_string(),
        description: value["description"].as_str().map(str::to_string),
    }
}

pub(crate) fn autocomplete_item_json(item: &AutocompleteItem) -> Value {
    let mut out = json!({ "value": item.value, "label": item.label });
    if let Some(d) = &item.description {
        out["description"] = json!(d);
    }
    out
}

/// A `$piRust` marker (`{ $piRust: {...} }`) of a function implemented in Rust.
pub(crate) fn rust_marker(value: &Value) -> Option<&Value> {
    value.get("$piRust")
}

/// Build a [`ToolDefinition`] from a JS tool descriptor (`describeTool` in bridge.js).
pub(crate) fn js_tool_definition(bridge: &Arc<Bridge>, tool: &Value) -> anyhow::Result<ToolDefinition> {
    let name = tool["name"].as_str().unwrap_or_default().to_string();
    let label = tool["label"].as_str().map(str::to_string).unwrap_or_else(|| name.clone());
    let description = tool["description"].as_str().unwrap_or_default().to_string();
    let parameters = tool["parameters"].clone();

    // Built-in tools (Rust markers) have TypeBox schemas in TS.
    let parameters_typebox = tool["parametersTypebox"].as_bool() == Some(true) || rust_marker(&tool["execute"]).is_some();
    let execute: ToolExecuteFn = match rust_marker(&tool["execute"]) {
        Some(marker) => builtin::rust_execute(bridge, marker)?,
        None => {
            let handle = bridge.handle(&tool["execute"]).ok_or_else(|| anyhow::anyhow!("Tool \"{name}\" must define execute()"))?;
            js_execute(bridge.clone(), handle)
        }
    };
    let mut def = ToolDefinition {
        name: name.clone(),
        label,
        description,
        prompt_snippet: tool["promptSnippet"].as_str().map(str::to_string),
        prompt_guidelines: opt(&tool["promptGuidelines"]).unwrap_or(None),
        parameters,
        parameters_typebox,
        constrained_sampling: if tool["constrainedSampling"].is_object() {
            serde_json::from_value(tool["constrainedSampling"].clone()).ok()
        } else {
            None
        },
        render_shell: opt(&tool["renderShell"]).unwrap_or(None),
        prepare_arguments: None,
        output_schema: if tool["outputSchema"].is_null() { None } else { Some(tool["outputSchema"].clone()) },
        exposure: opt(&tool["exposure"])?,
        namespace: opt(&tool["namespace"])?,
        reaches_tools: None,
        annotations: opt(&tool["annotations"])?,
        default_active: tool["defaultActive"].as_bool(),
        prepare_loadout: None,
        execution_mode: opt(&tool["executionMode"]).unwrap_or(None),
        execute,
        render_call: None,
        render_result: None,
    };
    match rust_marker(&tool["prepareArguments"]) {
        Some(marker) => def.prepare_arguments = builtin::rust_prepare_arguments(bridge, marker),
        None => {
            if let Some(h) = bridge.handle(&tool["prepareArguments"]) {
                def.prepare_arguments = Some(Arc::new(move |args: Value| h.call_blocking(vec![args.clone()]).unwrap_or(args)));
            }
        }
    }
    if let Some(h) = bridge.handle(&tool["reachesTools"]) {
        def.reaches_tools = Some(Arc::new(move |input: &pi_ai::types::JsonObject| {
            match h.call_blocking(vec![Value::Object(input.clone())]) {
                Ok(Value::String(s)) if s == "all" => ReachesTools::All(AllTools::All),
                Ok(v) => ReachesTools::Words(serde_json::from_value(v).unwrap_or_default()),
                Err(_) => ReachesTools::All(AllTools::All),
            }
        }));
    }
    if let Some(h) = bridge.handle(&tool["prepareLoadout"]) {
        let b = bridge.clone();
        def.prepare_loadout = Some(Arc::new(move |loadout: &dyn ToolLoadout| {
            let snapshot = loadout_snapshot(loadout);
            let value = b.glue_call_blocking("invokePrepareLoadout", vec![h.to_value(), snapshot]).ok()?;
            opt(&value).ok().flatten()
        }));
    }
    let (render_call, render_result) = component::tool_renderers(bridge, &tool["renderCall"], &tool["renderResult"])?;
    def.render_call = render_call;
    def.render_result = render_result;
    // `session.getToolDefinition()` hands back the extension's own tool object.
    if let Some(object) = bridge.handle(&tool["object"]) {
        bridge.origins.record(&def.execute, object);
    }
    Ok(def)
}

fn agent_tool_json(tool: &pi_agent::types::AgentToolRef) -> Value {
    json!({
        "name": tool.name(),
        "label": tool.label(),
        "description": tool.description(),
        "parameters": tool.parameters(),
    })
}

fn loadout_snapshot(loadout: &dyn ToolLoadout) -> Value {
    let registered = loadout.registered();
    let mut exposures = serde_json::Map::new();
    let mut namespaces = serde_json::Map::new();
    let mut guidelines = serde_json::Map::new();
    for tool in &registered {
        let name = tool.name().to_string();
        exposures.insert(name.clone(), json!(loadout.get_exposure(&name).as_str()));
        if let Some(ns) = loadout.get_namespace(&name) {
            namespaces.insert(name.clone(), serde_json::to_value(ns).unwrap_or(Value::Null));
        }
        guidelines.insert(name.clone(), json!(loadout.get_prompt_guidelines(&name)));
    }
    json!({
        "declared": loadout.declared().iter().map(agent_tool_json).collect::<Vec<_>>(),
        "callable": loadout.callable().iter().map(agent_tool_json).collect::<Vec<_>>(),
        "registered": registered.iter().map(agent_tool_json).collect::<Vec<_>>(),
        "exposures": exposures,
        "namespaces": namespaces,
        "guidelines": guidelines,
    })
}

/// `execute(toolCallId, params, signal, onUpdate, ctx)` of a JS tool.
pub(crate) fn js_execute(bridge: Arc<Bridge>, handle: JsHandle) -> ToolExecuteFn {
    Arc::new(move |tool_call_id, params, signal, on_update, ctx| {
        let bridge = bridge.clone();
        let handle = handle.clone();
        Box::pin(async move {
            let (signal_value, _guard) = match &signal {
                Some(token) => {
                    let (v, g) = bridge.signal_for(token);
                    (v, Some(g))
                }
                None => (Value::Null, None),
            };
            let update_id = on_update.map(|cb| {
                bridge.register_callback(Arc::new(move |args: Vec<Value>| {
                    let partial: pi_agent::types::AgentToolResult =
                        serde_json::from_value(args.into_iter().next().unwrap_or(Value::Null)).unwrap_or_default();
                    cb(partial);
                    Ok(Value::Null)
                }))
            });
            let ctx_id = ctx.map(|c| bridge.contexts.insert(CtxEntry::Tool(c)));
            let result = bridge
                .glue_call(
                    "executeTool",
                    vec![handle.to_value(), json!(tool_call_id), params, signal_value, json!(update_id), json!(ctx_id)],
                )
                .await;
            if let Some(id) = update_id {
                bridge.callbacks.remove(id);
            }
            if let Some(id) = ctx_id {
                bridge.detach_ctx(id, "tool");
            }
            let result = result?;
            if result.is_null() {
                return Ok(pi_agent::types::AgentToolResult::default());
            }
            serde_json::from_value(result).map_err(|e| anyhow::anyhow!("Invalid tool result: {e}"))
        })
    })
}

/// A `CustomEntry` as the TS object (with `type: "custom"`).
pub(crate) fn custom_entry_json(entry: &CustomEntry) -> Value {
    let mut value = serde_json::to_value(entry).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        let mut out = serde_json::Map::new();
        out.insert("type".into(), json!("custom"));
        out.extend(std::mem::take(map));
        return Value::Object(out);
    }
    value
}

