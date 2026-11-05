//! `ctx`, `ctx.ui`, `ctx.sessionManager` and `ctx.modelRegistry` of JavaScript extensions.

use std::sync::Arc;

use pi_jsrt::JsHandle;
use serde_json::{Value, json};

use super::api::{arg, opt, post_call, str_arg};
use super::{Bridge, CtxEntry, component, native, provider};
use crate::core::model_registry::ModelRegistryRegisterProviderProviderOrName;
use crate::core::extensions::types::*;
use crate::core::session_manager::SessionManager;
use crate::core::session_manager::NewSessionOptions as SmNewSessionOptions;

fn entry(bridge: &Bridge, payload: &Value) -> anyhow::Result<CtxEntry> {
    let id = payload["ctx"].as_u64().unwrap_or_default();
    bridge
        .contexts
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("This extension ctx is no longer available (it belonged to a finished call)."))
}

fn base(entry: &CtxEntry) -> anyhow::Result<&ExtensionContext> {
    entry.base().ok_or_else(|| anyhow::anyhow!("This property is not available in this context"))
}

/// `ctx.ui` of any context kind.
fn ui_of(entry: &CtxEntry) -> anyhow::Result<Arc<dyn ExtensionUIContext>> {
    match entry {
        CtxEntry::Trust(t) => Ok(t.ui.clone()),
        other => base(other)?.ui(),
    }
}

/// A session manager handed to JavaScript: host-owned ones are held weakly (they live as long
/// as pi's session uses them), ones created from JavaScript (`SessionManager.create()`) strongly
/// until the JS object is collected.
#[derive(Clone)]
pub(crate) enum SessionManagerRef {
    Host(std::sync::Weak<SessionManager>),
    Owned(Arc<SessionManager>),
}

/// The id of a host-owned session manager for JavaScript; one id (one JS object) per manager.
pub(crate) fn session_manager_id(bridge: &Bridge, sm: &Arc<SessionManager>) -> u64 {
    let key = Arc::as_ptr(sm) as usize;
    let mut ids = bridge.session_manager_ids.lock();
    if let Some(id) = ids.get(&key)
        && let Some(SessionManagerRef::Host(weak)) = bridge.session_managers.get(*id)
        && weak.upgrade().is_some_and(|existing| Arc::ptr_eq(&existing, sm))
    {
        return *id;
    }
    // Forget managers that are gone (their addresses may be reused).
    ids.retain(|_, id| match bridge.session_managers.get(*id) {
        Some(SessionManagerRef::Host(weak)) if weak.strong_count() > 0 => true,
        _ => {
            bridge.session_managers.remove(*id);
            false
        }
    });
    let id = bridge.session_managers.insert(SessionManagerRef::Host(Arc::downgrade(sm)));
    ids.insert(key, id);
    id
}

/// The session manager a `sm` host call targets (`{ sm: id }`).
pub(crate) fn session_manager_of(bridge: &Bridge, payload: &Value) -> anyhow::Result<Arc<SessionManager>> {
    let id = payload["sm"].as_u64().unwrap_or_default();
    match bridge.session_managers.get(id) {
        Some(SessionManagerRef::Owned(sm)) => Ok(sm),
        Some(SessionManagerRef::Host(weak)) => {
            weak.upgrade().ok_or_else(|| anyhow::anyhow!("This session manager is no longer available"))
        }
        None => Err(anyhow::anyhow!("This session manager is no longer available")),
    }
}

pub(crate) fn dispatch(bridge: &Arc<Bridge>, op: &str, payload: Value) -> anyhow::Result<Value> {
    match op {
        "ctx.get" => ctx_get(bridge, &payload),
        "ctx.release" => {
            bridge.contexts.remove(payload["ctx"].as_u64().unwrap_or_default());
            Ok(Value::Null)
        }
        "ctx.call" => ctx_call(bridge, &payload),
        "sm" => session_manager_call(bridge, &payload),
        "sm.static" => session_manager_static(bridge, &payload),
        "mr" => model_registry_call(bridge, &payload),
        "ui" => ui_call(bridge, &payload),
        _ => Err(anyhow::anyhow!("unknown host call {op}")),
    }
}

fn ctx_get(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let entry = entry(bridge, payload)?;
    let prop = str_arg(payload, "prop");
    if let CtxEntry::Trust(t) = &entry {
        return Ok(match prop.as_str() {
            "mode" => json!(t.mode.as_str()),
            "hasUI" => json!(t.has_ui),
            "cwd" => json!(t.cwd),
            _ => Value::Null,
        });
    }
    let ctx = base(&entry)?;
    Ok(match prop.as_str() {
        "assertActive" => {
            ctx.host().assert_active()?;
            Value::Null
        }
        "mode" => json!(ctx.mode()?.as_str()),
        "sessionManager" => json!(session_manager_id(bridge, &ctx.session_manager()?)),
        "hasUI" => json!(ctx.has_ui()?),
        "cwd" => json!(ctx.cwd()?),
        "model" => serde_json::to_value(ctx.model()?)?,
        "scopedModels" => serde_json::to_value(ctx.scoped_models()?)?,
        "thinkingLevel" => serde_json::to_value(ctx.thinking_level()?)?,
        "signal" => match ctx.signal()? {
            Some(token) => bridge.detached_signal_for(&token),
            None => Value::Null,
        },
        "tools" => match &entry {
            CtxEntry::Tool(t) => Value::Array(
                t.tools()?
                    .iter()
                    .map(|tool| {
                        json!({
                            "name": tool.name(),
                            "label": tool.label(),
                            "description": tool.description(),
                            "parameters": tool.parameters(),
                        })
                    })
                    .collect(),
            ),
            _ => Value::Null,
        },
        _ => Value::Null,
    })
}

fn ctx_call(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let entry = entry(bridge, payload)?;
    let method = str_arg(payload, "m");
    if method == "getSystemPromptOptions" {
        return match &entry {
            CtxEntry::Command(c) => Ok(serde_json::to_value(c.get_system_prompt_options()?)?),
            CtxEntry::Replaced(c) => Ok(serde_json::to_value(c.get_system_prompt_options()?)?),
            _ => Err(anyhow::anyhow!("getSystemPromptOptions is not available in this context")),
        };
    }
    let ctx = base(&entry)?;
    Ok(match method.as_str() {
        "isIdle" => json!(ctx.is_idle()?),
        "isProjectTrusted" => json!(ctx.is_project_trusted()?),
        "abort" => {
            ctx.abort()?;
            Value::Null
        }
        "hasPendingMessages" => json!(ctx.has_pending_messages()?),
        "shutdown" => {
            ctx.shutdown()?;
            Value::Null
        }
        "getContextUsage" => serde_json::to_value(ctx.get_context_usage()?)?,
        "getSystemPrompt" => json!(ctx.get_system_prompt()?),
        "compact" => {
            let options = arg(payload, 0);
            let on_complete = bridge.handle(&options["onComplete"]);
            let on_error = bridge.handle(&options["onError"]);
            ctx.compact(CompactOptions {
                custom_instructions: options["customInstructions"].as_str().map(str::to_string),
                on_complete: on_complete.map(|h| {
                    Arc::new(move |result: crate::core::compaction::compaction::CompactionResult| {
                        post_call(&h, None, vec![serde_json::to_value(&result).unwrap_or(Value::Null)]);
                    }) as Arc<dyn Fn(_) + Send + Sync>
                }),
                on_error: on_error.map(|h| {
                    Arc::new(move |error: Arc<anyhow::Error>| {
                        post_call(&h, None, vec![json!({ "message": error.to_string() })]);
                    }) as Arc<dyn Fn(_) + Send + Sync>
                }),
            })?;
            Value::Null
        }
        _ => return Err(anyhow::anyhow!("ctx.{method} is not available")),
    })
}

fn with_session_fn(bridge: &Arc<Bridge>, value: &Value) -> Option<WithSessionFn> {
    let handle = bridge.handle(value)?;
    let bridge = bridge.clone();
    Some(Arc::new(move |ctx: ReplacedSessionContext| {
        let bridge = bridge.clone();
        let handle = handle.clone();
        Box::pin(async move {
            let id = bridge.contexts.insert(CtxEntry::Replaced(ctx));
            bridge.glue_call("invokeWithSession", vec![handle.to_value(), json!(id)]).await?;
            Ok(())
        })
    }))
}

fn command_actions(entry: &CtxEntry) -> anyhow::Result<&ExtensionCommandContext> {
    match entry {
        CtxEntry::Command(c) => Ok(c),
        CtxEntry::Replaced(c) => Ok(&c.cmd),
        _ => Err(anyhow::anyhow!("This method is only available in command handlers")),
    }
}

pub(crate) async fn dispatch_async(bridge: Arc<Bridge>, op: &str, payload: Value) -> anyhow::Result<Value> {
    match op {
        "ctx.async" => ctx_async(&bridge, &payload).await,
        "ui.async" => ui_async(&bridge, &payload).await,
        "mr.async" => model_registry_async(&bridge, &payload).await,
        "sm.list" => session_manager_list(&bridge, &payload).await,
        "mr.stream" => {
            let registry = registry_of(&bridge, &payload)?;
            let method = str_arg(&payload, "m");
            let model = serde_json::from_value(arg(&payload, 0))?;
            let context = serde_json::from_value(arg(&payload, 1))?;
            let options = provider::stream_options_from_json(&bridge, &arg(&payload, 2));
            let stream = if method == "streamSimple" {
                registry.stream_simple(model, context, Some(options.into()))
            } else {
                registry.stream(model, context, Some(options.into()))
            };
            Ok(json!(provider::register_stream(&bridge, stream)))
        }
        _ => Err(anyhow::anyhow!("unknown async host call {op}")),
    }
}

async fn ctx_async(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let entry = entry(bridge, payload)?;
    let method = str_arg(payload, "m");
    match method.as_str() {
        "executeTool" => {
            let CtxEntry::Tool(tool_ctx) = &entry else {
                anyhow::bail!("executeTool is only available in tool contexts");
            };
            let name = arg(payload, 0).as_str().unwrap_or_default().to_string();
            let args = arg(payload, 1);
            let options = arg(payload, 2);
            let on_update = bridge.handle(&options["onUpdate"]).map(|h| {
                Arc::new(move |partial: pi_agent::types::AgentToolResult| {
                    post_call(&h, None, vec![serde_json::to_value(&partial).unwrap_or(Value::Null)]);
                }) as pi_agent::types::AgentToolUpdateCallback
            });
            let outcome = tool_ctx
                .execute_tool(&name, args, ExecuteToolOptions { signal: bridge.token(&options["signal"]), on_update })
                .await?;
            let mut out = json!({
                "toolCall": serde_json::to_value(&outcome.tool_call)?,
                "result": serde_json::to_value(&outcome.result)?,
                "isError": outcome.is_error,
            });
            if let Some(d) = outcome.duration_ms {
                out["durationMs"] = json!(d);
            }
            Ok(out)
        }
        "waitForIdle" => {
            command_actions(&entry)?.wait_for_idle().await?;
            Ok(Value::Null)
        }
        "newSession" => {
            let options = arg(payload, 0);
            let setup = bridge.handle(&options["setup"]).map(|h| {
                let bridge = bridge.clone();
                Arc::new(move |sm: Arc<SessionManager>| {
                    let bridge = bridge.clone();
                    let h = h.clone();
                    Box::pin(async move {
                        let id = session_manager_id(&bridge, &sm);
                        let r = bridge.glue_call("invokeSetup", vec![h.to_value(), json!(id)]).await;
                        drop(sm);
                        r.map(|_| ())
                    }) as futures::future::BoxFuture<'static, anyhow::Result<()>>
                }) as Arc<dyn Fn(Arc<SessionManager>) -> futures::future::BoxFuture<'static, anyhow::Result<()>> + Send + Sync>
            });
            let outcome = command_actions(&entry)?
                .new_session(NewSessionOptions {
                    parent_session: options["parentSession"].as_str().map(str::to_string),
                    setup,
                    with_session: with_session_fn(bridge, &options["withSession"]),
                })
                .await?;
            Ok(serde_json::to_value(outcome)?)
        }
        "fork" => {
            let entry_id = arg(payload, 0).as_str().unwrap_or_default().to_string();
            let options = arg(payload, 1);
            let outcome = command_actions(&entry)?
                .fork(
                    &entry_id,
                    ForkOptions {
                        position: opt(&options["position"])?,
                        with_session: with_session_fn(bridge, &options["withSession"]),
                    },
                )
                .await?;
            Ok(serde_json::to_value(outcome)?)
        }
        "navigateTree" => {
            let target_id = arg(payload, 0).as_str().unwrap_or_default().to_string();
            let options: NavigateTreeOptions = opt(&arg(payload, 1))?.unwrap_or_default();
            Ok(serde_json::to_value(command_actions(&entry)?.navigate_tree(&target_id, options).await?)?)
        }
        "switchSession" => {
            let path = arg(payload, 0).as_str().unwrap_or_default().to_string();
            let options = arg(payload, 1);
            let outcome = command_actions(&entry)?
                .switch_session(&path, SwitchSessionOptions { with_session: with_session_fn(bridge, &options["withSession"]) })
                .await?;
            Ok(serde_json::to_value(outcome)?)
        }
        "reload" => {
            command_actions(&entry)?.reload().await?;
            Ok(Value::Null)
        }
        "sendMessage" | "sendUserMessage" => {
            let CtxEntry::Replaced(replaced) = &entry else {
                anyhow::bail!("{method} is only available in withSession contexts");
            };
            if method == "sendMessage" {
                let message: CustomMessageInput = serde_json::from_value(arg(payload, 0))?;
                let options: SendMessageOptions = opt(&arg(payload, 1))?.unwrap_or_default();
                replaced.send_message(message, options).await?;
            } else {
                let content = serde_json::from_value(arg(payload, 0))?;
                let options: SendUserMessageOptions = opt(&arg(payload, 1))?.unwrap_or_default();
                replaced.send_user_message(content, options).await?;
            }
            Ok(Value::Null)
        }
        _ => Err(anyhow::anyhow!("ctx.{method} is not available")),
    }
}

/// A typed entry serialized on its own: the `type` tag comes from the enclosing enum.
fn with_type(tag: &str, mut value: Value) -> Value {
    if let Value::Object(map) = &mut value
        && !map.contains_key("type")
    {
        map.shift_insert(0, "type".to_string(), json!(tag));
    }
    value
}

fn usage_arg(value: &Value) -> anyhow::Result<Option<pi_ai::types::Usage>> {
    opt(value)
}

/// Methods of a `SessionManager` instance (TS `SessionManager`; `ctx.sessionManager` is the live
/// instance, typed `ReadonlySessionManager` but complete at runtime).
fn session_manager_call(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let sm = session_manager_of(bridge, payload)?;
    let method = str_arg(payload, "m");
    let s = |i: usize| arg(payload, i).as_str().map(str::to_string);
    let options = |i: usize| -> anyhow::Result<Option<SmNewSessionOptions>> { opt(&arg(payload, i)) };
    Ok(match method.as_str() {
        "setSessionFile" => {
            sm.set_session_file(&s(0).unwrap_or_default())?;
            Value::Null
        }
        "newSession" => json!(sm.new_session(options(0)?.as_ref())?),
        "isPersisted" => json!(sm.is_persisted()),
        "getCwd" => json!(sm.get_cwd()),
        "getSessionDir" => json!(sm.get_session_dir()),
        "usesDefaultSessionDir" => json!(sm.uses_default_session_dir()),
        "getSessionId" => json!(sm.get_session_id()),
        "getSessionFile" => json!(sm.get_session_file()),
        "appendMessage" => {
            let message: pi_agent::types::AgentMessage = serde_json::from_value(arg(payload, 0))?;
            json!(sm.append_message(message)?)
        }
        "appendThinkingLevelChange" => json!(sm.append_thinking_level_change(&s(0).unwrap_or_default())?),
        "appendModelChange" => json!(sm.append_model_change(&s(0).unwrap_or_default(), &s(1).unwrap_or_default())?),
        "appendUsage" => {
            let usage = serde_json::from_value(arg(payload, 3))?;
            let entry = sm.append_usage(
                &s(0).unwrap_or_default(),
                &s(1).unwrap_or_default(),
                &s(2).unwrap_or_default(),
                usage,
                s(4).as_deref(),
            )?;
            with_type("usage", serde_json::to_value(entry)?)
        }
        "appendCompaction" => json!(sm.append_compaction(
            &s(0).unwrap_or_default(),
            s(1).as_deref(),
            arg(payload, 2).as_f64().unwrap_or_default() as u64,
            Some(arg(payload, 3)).filter(|v| !v.is_null()),
            arg(payload, 4).as_bool(),
            usage_arg(&arg(payload, 5))?,
        )?),
        "appendCustomEntry" => json!(sm.append_custom_entry(&s(0).unwrap_or_default(), Some(arg(payload, 1)).filter(|v| !v.is_null()))?),
        "appendSessionInfo" => json!(sm.append_session_info(&s(0).unwrap_or_default())?),
        "getSessionName" => json!(sm.get_session_name()),
        "appendCustomMessageEntry" => json!(sm.append_custom_message_entry(
            &s(0).unwrap_or_default(),
            serde_json::from_value(arg(payload, 1))?,
            arg(payload, 2).as_bool().unwrap_or(false),
            Some(arg(payload, 3)).filter(|v| !v.is_null()),
        )?),
        "appendContextEdit" => {
            let replacement = arg(payload, 1);
            if !replacement.is_null()
                && !(replacement.is_object() && (replacement["content"].is_string() || replacement["content"].is_array()))
            {
                anyhow::bail!("Context edit replacement must be null or contain string/array content");
            }
            let replacement: Option<crate::core::session_manager::ContextEditEntryReplacement> = opt(&replacement)?;
            json!(sm.append_context_edit(&s(0).unwrap_or_default(), replacement.as_ref())?)
        }
        "getLeafId" => json!(sm.get_leaf_id()),
        "getLeafEntry" => serde_json::to_value(sm.get_leaf_entry())?,
        "getEntry" => serde_json::to_value(sm.get_entry(&s(0).unwrap_or_default()))?,
        "getChildren" => serde_json::to_value(sm.get_children(&s(0).unwrap_or_default()))?,
        "getLabel" => json!(sm.get_label(&s(0).unwrap_or_default())),
        "appendLabelChange" => json!(sm.append_label_change(&s(0).unwrap_or_default(), s(1).as_deref())?),
        "getBranch" => serde_json::to_value(sm.get_branch(s(0).as_deref()))?,
        "buildContextEntries" => serde_json::to_value(sm.build_context_entries())?,
        "buildSessionProjection" => serde_json::to_value(sm.build_session_projection())?,
        "buildSessionContext" => serde_json::to_value(sm.build_session_context())?,
        "getHeader" => with_type("session", serde_json::to_value(sm.get_header())?),
        "getEntries" => serde_json::to_value(sm.get_entries())?,
        "getEntryCount" => json!(sm.get_entry_count()),
        "getTree" => serde_json::to_value(sm.get_tree())?,
        "branch" => {
            sm.branch(&s(0).unwrap_or_default())?;
            Value::Null
        }
        "resetLeaf" => {
            sm.reset_leaf();
            Value::Null
        }
        "branchWithSummary" => json!(sm.branch_with_summary(
            s(0).as_deref(),
            &s(1).unwrap_or_default(),
            Some(arg(payload, 2)).filter(|v| !v.is_null()),
            arg(payload, 3).as_bool(),
            usage_arg(&arg(payload, 4))?,
        )?),
        "createBranchedSession" => json!(sm.create_branched_session(&s(0).unwrap_or_default())?),
        _ => return Err(anyhow::anyhow!("sessionManager.{method} is not a function")),
    })
}

/// Static `SessionManager` factories and helpers (`SessionManager.create()`, ...); new managers
/// are owned by their JavaScript object (`{ sm: id }`).
fn session_manager_static(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let method = str_arg(payload, "m");
    let s = |i: usize| arg(payload, i).as_str().map(str::to_string);
    let options = |i: usize| -> anyhow::Result<Option<SmNewSessionOptions>> { opt(&arg(payload, i)) };
    let entries = |i: usize| -> anyhow::Result<Option<Vec<crate::core::session_manager::FileEntry>>> { opt(&arg(payload, i)) };
    let owned = |sm: Arc<SessionManager>| json!({ "sm": bridge.session_managers.insert(SessionManagerRef::Owned(sm)) });
    let cwd = || s(0).unwrap_or_else(|| bridge.cwd.clone());
    Ok(match method.as_str() {
        "new" => owned(Arc::new(SessionManager::new(
            &cwd(),
            &s(1).unwrap_or_default(),
            s(2).as_deref(),
            arg(payload, 3).as_bool().unwrap_or(false),
            options(4)?.as_ref(),
            entries(5)?,
        )?)),
        "create" => owned(SessionManager::create(&cwd(), s(1).as_deref(), options(2)?.as_ref())?),
        "open" => owned(SessionManager::open(&s(0).unwrap_or_default(), s(1).as_deref(), s(2).as_deref())?),
        "continueRecent" => owned(SessionManager::continue_recent(&cwd(), s(1).as_deref())?),
        "inMemory" => owned(SessionManager::in_memory(Some(&cwd()), options(1)?.as_ref(), entries(2)?)?),
        "forkFrom" => owned(SessionManager::fork_from(
            &s(0).unwrap_or_default(),
            &s(1).unwrap_or_default(),
            s(2).as_deref(),
            options(3)?.as_ref(),
        )?),
        "findById" => json!(SessionManager::find_by_id(&cwd(), &s(1).unwrap_or_default(), s(2).as_deref())?),
        "release" => {
            if let Some(id) = payload["id"].as_u64()
                && let Some(SessionManagerRef::Owned(_)) = bridge.session_managers.get(id)
            {
                bridge.session_managers.remove(id);
            }
            Value::Null
        }
        _ => return Err(anyhow::anyhow!("SessionManager.{method} is not a function")),
    })
}

/// `SessionManager.list()` / `listAll()`.
async fn session_manager_list(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let method = str_arg(payload, "m");
    let s = |i: usize| arg(payload, i).as_str().map(str::to_string);
    let on_progress = bridge.handle(&payload["onProgress"]).map(|h| {
        Arc::new(move |loaded: i64, total: i64, partial: Option<Vec<crate::core::session_manager::SessionInfo>>| {
            let mut args = vec![json!(loaded), json!(total)];
            if let Some(partial) = partial {
                args.push(serde_json::to_value(partial).unwrap_or(Value::Null));
            }
            post_call(&h, None, args);
        }) as crate::core::session_manager::SessionListProgress
    });
    let signal = bridge.token(&payload["signal"]);
    let sessions = if method == "listAll" {
        SessionManager::list_all(s(0).as_deref(), on_progress, signal).await?
    } else {
        SessionManager::list(&s(0).unwrap_or_else(|| bridge.cwd.clone()), s(1).as_deref(), on_progress, signal).await?
    };
    Ok(serde_json::to_value(sessions)?)
}

/// The model registry a `mr` host call targets: `ctx.modelRegistry` (`{ ctx }`) or one created
/// with `new ModelRegistry(runtime)` (`{ registry }`).
pub(crate) fn registry_of(bridge: &Bridge, payload: &Value) -> anyhow::Result<Arc<crate::core::model_registry::ModelRegistry>> {
    if let Some(id) = payload["registry"].as_u64() {
        return bridge.model_registries.get(id).ok_or_else(|| anyhow::anyhow!("This model registry is no longer available"));
    }
    base(&entry(bridge, payload)?)?.model_registry()
}

fn model_registry_call(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    if str_arg(payload, "m") == "new" {
        let runtime = super::sdk::runtime_of(bridge, &json!({ "runtime": arg(payload, 0) }))?;
        let registry = Arc::new(crate::core::model_registry::ModelRegistry::new(runtime));
        return Ok(json!(bridge.model_registries.insert(registry)));
    }
    if str_arg(payload, "m") == "release" {
        if let Some(id) = payload["registry"].as_u64() {
            bridge.model_registries.remove(id);
        }
        return Ok(Value::Null);
    }
    let registry = registry_of(bridge, payload)?;
    let method = str_arg(payload, "m");
    let s = |i: usize| arg(payload, i).as_str().unwrap_or_default().to_string();
    Ok(match method.as_str() {
        "find" => serde_json::to_value(registry.find(&s(0), &s(1)))?,
        "findOfType" => serde_json::to_value(registry.find_of_type(serde_json::from_value(arg(payload, 0))?, &s(1), &s(2)))?,
        "getModelOfType" => {
            serde_json::to_value(registry.get_model_of_type(serde_json::from_value(arg(payload, 0))?, &s(1), &s(2)))?
        }
        "getModelsOfType" => serde_json::to_value(
            registry.get_models_of_type(serde_json::from_value(arg(payload, 0))?, arg(payload, 1).as_str()),
        )?,
        "getAll" => serde_json::to_value(registry.get_all())?,
        "getAvailable" => serde_json::to_value(registry.get_available())?,
        "getError" => json!(registry.get_error()),
        "hasConfiguredAuth" => json!(registry.has_configured_auth(&serde_json::from_value(arg(payload, 0))?)),
        "isUsingOAuth" => json!(registry.is_using_o_auth(&serde_json::from_value(arg(payload, 0))?)),
        "getProviderAuthStatus" => serde_json::to_value(registry.get_provider_auth_status(&s(0)))?,
        "getProviderDisplayName" => json!(registry.get_provider_display_name(&s(0))),
        "getRegisteredProviderIds" => json!(registry.get_registered_provider_ids()),
        "runtime" => json!(super::sdk::runtime_id(bridge, registry.runtime())),
        "getProvider" => match registry.get_provider(&s(0)) {
            Some(p) => native::provider_to_js(bridge, p),
            None => Value::Null,
        },
        "getRegisteredNativeProvider" => match registry.get_registered_native_provider(&s(0)) {
            Some(p) => native::provider_to_js(bridge, p),
            None => Value::Null,
        },
        "getRegisteredProviderConfig" => match registry.get_registered_provider_config_revision(&s(0)) {
            Some((rev, config)) => json!({ "rev": rev, "config": native::provider_config_to_js(bridge, &config) }),
            None => Value::Null,
        },
        "registerProvider" => {
            let registration = arg(payload, 0);
            match registration["name"].as_str() {
                Some(name) => {
                    let config = provider::js_provider_config(bridge, &registration["config"])?;
                    registry.register_provider(&ModelRegistryRegisterProviderProviderOrName::String(name.to_string()), Some(&config))?;
                }
                None => {
                    let native = native::native_provider(bridge, &registration["provider"])?;
                    registry.register_provider(&ModelRegistryRegisterProviderProviderOrName::Arc(native), None)?;
                }
            }
            Value::Null
        }
        "unregisterProvider" => {
            registry.unregister_provider(&s(0));
            Value::Null
        }
        "registerVirtualModel" => {
            registry.register_virtual_model(&provider::js_virtual_model_definition(bridge, &arg(payload, 0))?)?;
            Value::Null
        }
        "unregisterVirtualModel" => {
            registry.unregister_virtual_model(&s(0), &s(1));
            Value::Null
        }
        _ => return Err(anyhow::anyhow!("modelRegistry.{method} is not a function")),
    })
}

async fn model_registry_async(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let registry = registry_of(bridge, payload)?;
    let method = str_arg(payload, "m");
    Ok(match method.as_str() {
        "refresh" => {
            let options = arg(payload, 0);
            let result = registry
                .refresh(Some(pi_ai::models::ModelsRefreshOptions {
                    allow_network: options["allowNetwork"].as_bool(),
                    providers: opt(&options["providers"])?,
                    force: options["force"].as_bool(),
                    signal: bridge.token(&options["signal"]),
                }))
                .await?;
            json!({ "aborted": result.aborted, "errors": result.errors })
        }
        "getApiKeyAndHeaders" => {
            let model = serde_json::from_value(arg(payload, 0))?;
            serde_json::to_value(registry.get_api_key_and_headers(&model).await)?
        }
        "complete" => {
            let model = serde_json::from_value(arg(payload, 0))?;
            let context = serde_json::from_value(arg(payload, 1))?;
            let options = provider::stream_options_from_json(bridge, &arg(payload, 2));
            serde_json::to_value(registry.complete(model, context, Some(options.into())).await)?
        }
        "getApiKeyForProvider" => json!(registry.get_api_key_for_provider(arg(payload, 0).as_str().unwrap_or_default()).await),
        "getProviderAuth" => match registry.get_provider_auth(arg(payload, 0).as_str().unwrap_or_default()).await? {
            Some(auth) => native::auth_result_to_js(&auth),
            None => Value::Null,
        },
        "getAvailableOfType" => {
            let options = crate::compat::AuthOperationOptions { signal: bridge.token(&arg(payload, 2)["signal"]) };
            serde_json::to_value(
                registry
                    .get_available_of_type(serde_json::from_value(arg(payload, 0))?, arg(payload, 1).as_str(), Some(&options))
                    .await?,
            )?
        }
        "classify" => {
            let model = serde_json::from_value(arg(payload, 0))?;
            let context = serde_json::from_value(arg(payload, 1))?;
            let options = provider::stream_options_from_json(bridge, &arg(payload, 2));
            serde_json::to_value(registry.classify(model, context, Some(options.into())).await)?
        }
        "generateImages" => {
            let model = serde_json::from_value(arg(payload, 0))?;
            let context = serde_json::from_value(arg(payload, 1))?;
            let options = provider::stream_options_from_json(bridge, &arg(payload, 2));
            serde_json::to_value(registry.generate_images(model, context, Some(options.into())).await)?
        }
        _ => return Err(anyhow::anyhow!("modelRegistry.{method} is not a function")),
    })
}

fn dialog_options(bridge: &Bridge, value: &Value) -> ExtensionUIDialogOptions {
    ExtensionUIDialogOptions { signal: bridge.token(&value["signal"]), timeout: value["timeout"].as_u64() }
}

async fn ui_async(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let entry = entry(bridge, payload)?;
    let ui = ui_of(&entry)?;
    let method = str_arg(payload, "m");
    let s = |i: usize| arg(payload, i).as_str().map(str::to_string);
    Ok(match method.as_str() {
        "select" => {
            let options: Vec<String> = opt(&arg(payload, 1))?.unwrap_or_default();
            json!(ui.select(&s(0).unwrap_or_default(), options, dialog_options(bridge, &arg(payload, 2))).await)
        }
        "confirm" => json!(
            ui.confirm(&s(0).unwrap_or_default(), &s(1).unwrap_or_default(), dialog_options(bridge, &arg(payload, 2)))
                .await
        ),
        "input" => {
            json!(ui.input(&s(0).unwrap_or_default(), s(1).as_deref(), dialog_options(bridge, &arg(payload, 2))).await)
        }
        "editor" => json!(ui.editor(&s(0).unwrap_or_default(), s(1).as_deref()).await),
        "custom" => {
            let factory = bridge.handle(&arg(payload, 0)).ok_or_else(|| anyhow::anyhow!("ui.custom() expects a factory"))?;
            let options = arg(payload, 1);
            let custom_options = component::custom_ui_options(bridge, &options);
            let factory = component::js_custom_factory(bridge.clone(), factory);
            let result = ui.custom(factory, custom_options).await?;
            match result.downcast::<component::JsUiValue>() {
                Ok(v) => v.0,
                Err(_) => Value::Null,
            }
        }
        _ => return Err(anyhow::anyhow!("ui.{method} is not available")),
    })
}

fn ui_call(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<Value> {
    let entry = entry(bridge, payload)?;
    let ui = ui_of(&entry)?;
    let method = str_arg(payload, "m");
    let s = |i: usize| arg(payload, i).as_str().map(str::to_string);
    match method.as_str() {
        "notify" => {
            let message = s(0).unwrap_or_default();
            let kind: NotifyType = opt(&arg(payload, 1))?.unwrap_or_default();
            bridge.ui_dispatch(move || ui.notify(&message, kind));
        }
        "setStatus" => {
            let (key, text) = (s(0).unwrap_or_default(), s(1));
            bridge.ui_dispatch(move || ui.set_status(&key, text.as_deref()));
        }
        "setWorkingMessage" => {
            let message = s(0);
            bridge.ui_dispatch(move || ui.set_working_message(message.as_deref()));
        }
        "setWorkingVisible" => {
            let visible = arg(payload, 0).as_bool().unwrap_or(false);
            bridge.ui_dispatch(move || ui.set_working_visible(visible));
        }
        "setWorkingIndicator" => {
            let options: Option<WorkingIndicatorOptions> = opt(&arg(payload, 0))?;
            bridge.ui_dispatch(move || ui.set_working_indicator(options));
        }
        "setHiddenThinkingLabel" => {
            let label = s(0);
            bridge.ui_dispatch(move || ui.set_hidden_thinking_label(label.as_deref()));
        }
        "setTitle" => {
            let title = s(0).unwrap_or_default();
            bridge.ui_dispatch(move || ui.set_title(&title));
        }
        "pasteToEditor" => {
            let text = s(0).unwrap_or_default();
            bridge.ui_dispatch(move || ui.paste_to_editor(&text));
        }
        "setEditorText" => {
            let text = s(0).unwrap_or_default();
            bridge.ui_dispatch(move || ui.set_editor_text(&text));
        }
        "getEditorText" => return Ok(json!(bridge.ui_dispatch_wait(move || ui.get_editor_text())?)),
        "getToolsExpanded" => return Ok(json!(bridge.ui_dispatch_wait(move || ui.get_tools_expanded())?)),
        "setToolsExpanded" => {
            let expanded = arg(payload, 0).as_bool().unwrap_or(false);
            bridge.ui_dispatch(move || ui.set_tools_expanded(expanded));
        }
        "setWidget" => {
            let key = s(0).unwrap_or_default();
            let content = match arg(payload, 1) {
                Value::Null => None,
                Value::Array(lines) => {
                    Some(WidgetContent::Lines(lines.iter().map(|l| l.as_str().unwrap_or_default().to_string()).collect()))
                }
                other => match bridge.handle(&other) {
                    Some(factory) => Some(WidgetContent::Component(component::js_component_factory(bridge.clone(), factory))),
                    None => Some(WidgetContent::Lines(vec![other.as_str().unwrap_or_default().to_string()])),
                },
            };
            let options: ExtensionWidgetOptions = opt(&arg(payload, 2))?.unwrap_or_default();
            bridge.ui_dispatch(move || ui.set_widget(&key, content, options));
        }
        "setFooter" => {
            let factory = bridge.handle(&arg(payload, 0)).map(|f| component::js_footer_factory(bridge.clone(), f));
            bridge.ui_dispatch(move || ui.set_footer(factory));
        }
        "setHeader" => {
            let factory = bridge.handle(&arg(payload, 0)).map(|f| component::js_component_factory(bridge.clone(), f));
            bridge.ui_dispatch(move || ui.set_header(factory));
        }
        "setEditorComponent" => {
            let factory = bridge.handle(&arg(payload, 0)).map(|f| component::js_editor_factory(bridge.clone(), f));
            bridge.ui_dispatch(move || ui.set_editor_component(factory));
        }
        "getEditorComponent" => return Ok(json!(bridge.ui_dispatch_wait(move || ui.get_editor_component().is_some())?)),
        "addAutocompleteProvider" => {
            let factory = bridge
                .handle(&arg(payload, 0))
                .ok_or_else(|| anyhow::anyhow!("addAutocompleteProvider() expects a factory"))?;
            let factory = component::js_autocomplete_factory(bridge.clone(), factory);
            bridge.ui_dispatch(move || ui.add_autocomplete_provider(factory));
        }
        "onTerminalInput" => {
            let handler: JsHandle =
                bridge.handle(&arg(payload, 0)).ok_or_else(|| anyhow::anyhow!("onTerminalInput() expects a handler"))?;
            let unsubscribe = ui.on_terminal_input(Arc::new(move |data: &str| {
                let value = handler.call_blocking(vec![json!(data)]).ok()?;
                opt::<TerminalInputHandlerResult>(&value).ok().flatten()
            }));
            return Ok(json!(bridge.unsubscribes.insert(unsubscribe)));
        }
        "theme" => {
            let theme = bridge.ui_dispatch_wait(move || ui.theme())?;
            return Ok(json!(component::theme_id(bridge, &theme)));
        }
        "getAllThemes" => return Ok(serde_json::to_value(bridge.ui_dispatch_wait(move || ui.get_all_themes())?)?),
        "getTheme" => {
            let name = s(0).unwrap_or_default();
            let theme = bridge.ui_dispatch_wait(move || ui.get_theme(&name))?;
            return Ok(match theme {
                Some(t) => json!(component::theme_id(bridge, &t)),
                None => Value::Null,
            });
        }
        "setTheme" => {
            let selection = match arg(payload, 0) {
                Value::String(name) => ThemeSelection::Name(name),
                other => {
                    let id = other["$themeId"].as_u64().unwrap_or_default();
                    let theme = bridge
                        .components
                        .themes
                        .get(id)
                        .ok_or_else(|| anyhow::anyhow!("setTheme() expects a theme name or a theme from ctx.ui"))?;
                    ThemeSelection::Theme(theme)
                }
            };
            return Ok(serde_json::to_value(bridge.ui_dispatch_wait(move || ui.set_theme(selection))?)?);
        }
        _ => return Err(anyhow::anyhow!("ui.{method} is not available")),
    }
    Ok(Value::Null)
}
