//! Providers, virtual models and pi-ai streaming for JavaScript extensions.

use std::sync::Arc;

use futures::future::BoxFuture;
use pi_ai::types::{Model, StreamOptions, TranscriptContext};
use pi_ai::utils::event_stream::AssistantMessageEventStream;
use pi_jsrt::JsHandle;
use serde_json::{Map, Value, json};

use super::api::{opt, str_arg};
use super::{AsyncRustCallback, Bridge, CtxEntry, tokio_spawn};
use crate::compat::OAuthLoginCallbacks;
use crate::core::extensions::types::*;
use crate::core::virtual_models::{ModelRoute, ModelRouteRequest};

/// Bytes of a JS `Buffer` / `Uint8Array` (array of numbers), `{ $bytes }` marker, or string.
pub(crate) fn bytes_of(value: &Value) -> Vec<u8> {
    match value {
        Value::String(s) => s.clone().into_bytes(),
        Value::Array(a) => a.iter().map(|v| v.as_u64().unwrap_or(0) as u8).collect(),
        Value::Object(m) => {
            if let Some(inner) = m.get("$bytes").or_else(|| m.get("data")) {
                return bytes_of(inner);
            }
            // Typed arrays converted as objects keyed by index.
            let mut entries: Vec<(usize, u8)> =
                m.iter().filter_map(|(k, v)| Some((k.parse().ok()?, v.as_u64().unwrap_or(0) as u8))).collect();
            entries.sort_by_key(|(k, _)| *k);
            entries.into_iter().map(|(_, b)| b).collect()
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Telemetry (pi-ai `telemetry.ts`)
// ---------------------------------------------------------------------------

/// Telemetry parents (`StreamOptions.telemetry`) passed to JavaScript as `{ __piTelemetry: id }`,
/// alive while the JavaScript call that received them runs.
static TELEMETRY_PARENTS: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<u64, pi_telemetry::ContextRef>>> =
    std::sync::LazyLock::new(Default::default);
/// Request observers started by JavaScript `Models` collections, until their result is observed.
static TELEMETRY_OBSERVERS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<u64, pi_ai::api::lazy::LazyStreamObserver>>,
> = std::sync::LazyLock::new(Default::default);

/// Register a telemetry parent for JavaScript; release it with [`release_telemetry_parent`].
pub(crate) fn register_telemetry_parent(span: pi_telemetry::ContextRef) -> u64 {
    let id = super::next_id();
    TELEMETRY_PARENTS.lock().insert(id, span);
    id
}

pub(crate) fn release_telemetry_parent(id: u64) {
    TELEMETRY_PARENTS.lock().remove(&id);
}

/// The telemetry parent a JavaScript options object carries (`{ __piTelemetry: id }`).
fn telemetry_parent_of(value: &Value) -> Option<pi_telemetry::ContextRef> {
    let id = value.get("__piTelemetry")?.as_u64()?;
    TELEMETRY_PARENTS.lock().get(&id).cloned()
}

/// `observeModelRequest(model, options, poll)` for JavaScript `Models` collections: pi-ai's
/// request observer under the caller's telemetry parent. `null` (the no-op observer) without one.
fn telemetry_observe(payload: &Value) -> anyhow::Result<Value> {
    let Some(parent) = telemetry_parent_of(&payload["telemetry"]) else { return Ok(Value::Null) };
    let model: Model = serde_json::from_value(payload["model"].clone())?;
    let mut options = StreamOptions { telemetry: Some(parent), ..Default::default() };
    if let Some(reasoning) = payload.get("reasoning").filter(|v| !v.is_null()) {
        match serde_json::from_value(reasoning.clone()) {
            Ok(level) => options.reasoning = Some(level),
            Err(_) => {
                options.extra.insert("reasoning".to_string(), reasoning.clone());
            }
        }
    }
    let observer = pi_ai::telemetry::observe_model_request(&model, Some(&options), payload["poll"].as_bool() == Some(true));
    let id = super::next_id();
    TELEMETRY_OBSERVERS.lock().insert(id, observer);
    Ok(json!(id))
}

fn telemetry_event(payload: &Value) -> anyhow::Result<Value> {
    let id = payload["id"].as_u64().unwrap_or_default();
    let on_event = TELEMETRY_OBSERVERS.lock().get(&id).and_then(|o| o.on_event.clone());
    if let Some(on_event) = on_event
        && let Ok(event) = serde_json::from_value::<pi_ai::types::AssistantMessageEvent>(payload["event"].clone())
    {
        on_event(&event);
    }
    Ok(Value::Null)
}

fn telemetry_result(payload: &Value) -> anyhow::Result<Value> {
    let id = payload["id"].as_u64().unwrap_or_default();
    let observer = TELEMETRY_OBSERVERS.lock().remove(&id);
    if let Some(on_result) = observer.and_then(|o| o.on_result) {
        let message = serde_json::from_value::<pi_ai::types::AssistantMessage>(payload["message"].clone()).ok();
        on_result(message.as_ref());
    }
    Ok(Value::Null)
}

/// `StreamOptions` as the TS options object (functions and the signal are passed separately).
pub(crate) fn stream_options_to_json(options: &StreamOptions) -> Value {
    let mut out = Map::new();
    let mut put = |key: &str, value: Value| {
        if !value.is_null() {
            out.insert(key.to_string(), value);
        }
    };
    put("apiKey", json!(options.api_key));
    put("headers", serde_json::to_value(&options.headers).unwrap_or(Value::Null));
    put("timeoutMs", json!(options.timeout_ms));
    put("maxRetries", json!(options.max_retries));
    put("maxRetryDelayMs", json!(options.max_retry_delay_ms));
    put("temperature", json!(options.temperature));
    put("samplingParams", serde_json::to_value(&options.sampling_params).unwrap_or(Value::Null));
    put("maxTokens", json!(options.max_tokens));
    put("transport", serde_json::to_value(options.transport).unwrap_or(Value::Null));
    put("cacheRetention", serde_json::to_value(options.cache_retention).unwrap_or(Value::Null));
    put("sessionId", json!(options.session_id));
    put("metadata", serde_json::to_value(&options.metadata).unwrap_or(Value::Null));
    put("toolChoice", serde_json::to_value(options.tool_choice).unwrap_or(Value::Null));
    put("reasoning", serde_json::to_value(options.reasoning).unwrap_or(Value::Null));
    put("thinkingBudgets", serde_json::to_value(&options.thinking_budgets).unwrap_or(Value::Null));
    put("env", serde_json::to_value(&options.env).unwrap_or(Value::Null));
    for (k, v) in &options.extra {
        out.insert(k.clone(), v.clone());
    }
    Value::Object(out)
}

/// TS stream options (from JS) as `StreamOptions`. Unknown keys go to `extra`.
pub(crate) fn stream_options_from_json(bridge: &Bridge, value: &Value) -> StreamOptions {
    let mut options = StreamOptions::default();
    let Some(map) = value.as_object() else { return options };
    for (key, v) in map {
        if v.is_null() {
            continue;
        }
        match key.as_str() {
            "signal" => options.signal = bridge.token(v),
            "apiKey" => options.api_key = v.as_str().map(str::to_string),
            "headers" => options.headers = serde_json::from_value(v.clone()).ok(),
            "timeoutMs" => options.timeout_ms = v.as_u64(),
            "maxRetries" => options.max_retries = v.as_u64().map(|n| n as u32),
            "maxRetryDelayMs" => options.max_retry_delay_ms = v.as_u64(),
            "temperature" => options.temperature = v.as_f64(),
            "samplingParams" => options.sampling_params = serde_json::from_value(v.clone()).ok(),
            "maxTokens" => options.max_tokens = v.as_u64(),
            "transport" => options.transport = serde_json::from_value(v.clone()).ok(),
            "cacheRetention" => options.cache_retention = serde_json::from_value(v.clone()).ok(),
            "sessionId" => options.session_id = v.as_str().map(str::to_string),
            "metadata" => options.metadata = v.as_object().cloned(),
            "toolChoice" => options.tool_choice = serde_json::from_value(v.clone()).ok(),
            "reasoning" => options.reasoning = serde_json::from_value(v.clone()).ok(),
            "thinkingBudgets" => options.thinking_budgets = serde_json::from_value(v.clone()).ok(),
            "env" => options.env = serde_json::from_value(v.clone()).ok(),
            "telemetry" if v.get("__piTelemetry").is_some() => options.telemetry = telemetry_parent_of(v),
            _ => {
                options.extra.insert(key.clone(), v.clone());
            }
        }
    }
    options
}

/// Keep a Rust stream for JS to pull from (`stream.next`).
pub(crate) fn register_stream(bridge: &Arc<Bridge>, mut stream: AssistantMessageEventStream) -> u64 {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio_spawn(async move {
        while let Some(event) = stream.next_event().await {
            if tx.send(serde_json::to_value(&event).unwrap_or(Value::Null)).is_err() {
                break;
            }
        }
    });
    bridge.streams.insert(Arc::new(tokio::sync::Mutex::new(rx)))
}

/// `streamSimple(model, context, options)` implemented by JS.
fn js_stream_simple(bridge: Arc<Bridge>, handle: JsHandle) -> StreamSimpleFn {
    Arc::new(move |model: Model, context: TranscriptContext, options: Option<pi_ai::types::SimpleStreamOptions>| {
        let context = serde_json::to_value(&context).unwrap_or(Value::Null);
        super::native::js_stream(bridge.clone(), handle.clone(), None, model, vec![context], options.unwrap_or_default())
    })
}

/// Login callbacks of an OAuth flow as a JS object (`makeOAuthCallbacks` in bridge.js).
fn oauth_callbacks_json(bridge: &Arc<Bridge>, callbacks: Arc<dyn OAuthLoginCallbacks>) -> Value {
    let c = callbacks.clone();
    let on_auth = bridge.register_callback(Arc::new(move |args: Vec<Value>| {
        c.on_auth(serde_json::from_value(args.into_iter().next().unwrap_or(Value::Null)).unwrap_or_default());
        Ok(Value::Null)
    }));
    let c = callbacks.clone();
    let on_device_code = bridge.register_callback(Arc::new(move |args: Vec<Value>| {
        c.on_device_code(serde_json::from_value(args.into_iter().next().unwrap_or(Value::Null)).unwrap_or_default());
        Ok(Value::Null)
    }));
    let c = callbacks.clone();
    let on_progress = bridge.register_callback(Arc::new(move |args: Vec<Value>| {
        c.on_progress(args.first().and_then(Value::as_str).unwrap_or_default());
        Ok(Value::Null)
    }));
    let c = callbacks.clone();
    let on_prompt = bridge.async_callbacks.insert(Arc::new(move |args: Vec<Value>| {
        let c = c.clone();
        Box::pin(async move {
            let prompt = serde_json::from_value(args.into_iter().next().unwrap_or(Value::Null)).unwrap_or_default();
            Ok(json!(c.on_prompt(prompt).await?))
        }) as BoxFuture<'static, anyhow::Result<Value>>
    }) as AsyncRustCallback);
    let c = callbacks.clone();
    let on_select = bridge.async_callbacks.insert(Arc::new(move |args: Vec<Value>| {
        let c = c.clone();
        Box::pin(async move {
            let prompt = serde_json::from_value(args.into_iter().next().unwrap_or(Value::Null)).unwrap_or_default();
            Ok(json!(c.on_select(prompt).await?))
        }) as BoxFuture<'static, anyhow::Result<Value>>
    }) as AsyncRustCallback);
    let c = callbacks.clone();
    let on_manual = bridge.async_callbacks.insert(Arc::new(move |_args: Vec<Value>| {
        let c = c.clone();
        Box::pin(async move {
            match c.on_manual_code_input().await {
                Some(r) => Ok(json!(r?)),
                None => Ok(Value::Null),
            }
        }) as BoxFuture<'static, anyhow::Result<Value>>
    }) as AsyncRustCallback);
    let signal = callbacks.signal().map(|t| bridge.detached_signal_for(&t)).unwrap_or(Value::Null);
    json!({
        "onAuth": on_auth,
        "onDeviceCode": on_device_code,
        "onProgress": on_progress,
        "onPrompt": on_prompt,
        "onSelect": on_select,
        "onManualCodeInput": on_manual,
        "signal": signal,
    })
}

fn js_oauth(bridge: &Arc<Bridge>, value: &Value) -> Option<ProviderOAuthConfig> {
    let login = bridge.handle(&value["login"])?;
    let refresh = bridge.handle(&value["refreshToken"])?;
    let get_api_key = bridge.handle(&value["getApiKey"])?;
    let modify_models = bridge.handle(&value["modifyModels"]);
    let object = bridge.handle(&value["object"]);
    let b = bridge.clone();
    let b2 = bridge.clone();
    let config = ProviderOAuthConfig {
        name: value["name"].as_str().unwrap_or_default().to_string(),
        is_subscription: value["isSubscription"].as_bool(),
        uses_callback_server: value["usesCallbackServer"].as_bool(),
        login: Arc::new(move |callbacks| {
            let b = b.clone();
            let login = login.clone();
            Box::pin(async move {
                let callbacks = oauth_callbacks_json(&b, callbacks);
                let result = b.glue_call("invokeOAuthLogin", vec![login.to_value(), callbacks]).await?;
                Ok(serde_json::from_value(result)?)
            })
        }),
        refresh_token: Arc::new(move |credentials, signal| {
            let b = b2.clone();
            let refresh = refresh.clone();
            Box::pin(async move {
                let (signal_value, _guard) = b.signal_for(&signal);
                let result = b
                    .glue_call("invokeWithSignal", vec![refresh.to_value(), json!([serde_json::to_value(&credentials)?]), signal_value])
                    .await?;
                Ok(serde_json::from_value(result)?)
            })
        }),
        get_api_key: Arc::new(move |credentials| {
            get_api_key
                .call_blocking(vec![serde_json::to_value(credentials).unwrap_or(Value::Null)])
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        }),
        modify_models: modify_models.map(|h| {
            Arc::new(move |models: Vec<Model>, credentials: &pi_ai::auth::types::OAuthCredentials| {
                let args = vec![serde_json::to_value(&models).unwrap_or(Value::Null), serde_json::to_value(credentials).unwrap_or(Value::Null)];
                h.call_blocking(args).ok().and_then(|v| serde_json::from_value(v).ok()).unwrap_or(models)
            }) as Arc<dyn Fn(Vec<Model>, &pi_ai::auth::types::OAuthCredentials) -> Vec<Model> + Send + Sync>
        }),
    };
    // `getRegisteredProviderConfig()` hands back the extension's own oauth object.
    if let Some(object) = object {
        bridge.origins.record(&config.login, object);
    }
    Some(config)
}

/// `ProviderConfig` from the JS config object of `pi.registerProvider(name, config)`.
pub(crate) fn js_provider_config(bridge: &Arc<Bridge>, value: &Value) -> anyhow::Result<ProviderConfig> {
    let models: Option<Vec<ProviderModelConfig>> = match &value["models"] {
        Value::Null => None,
        v => Some(serde_json::from_value(v.clone()).map_err(|e| anyhow::anyhow!("Invalid provider models: {e}"))?),
    };
    let refresh_handle = bridge.handle(&value["refreshModels"]);
    let refresh_models = refresh_handle.clone().map(|h| {
        let b = bridge.clone();
        Arc::new(move |ctx: pi_ai::models::RefreshModelsContext| {
            let b = b.clone();
            let h = h.clone();
            Box::pin(async move {
                let (context, _guard, publish_id) = super::native::refresh_context_to_js(&b, &ctx);
                let result = b.glue_call("invokeRefreshModelsFn", vec![h.to_value(), context]).await;
                b.async_callbacks.remove(publish_id);
                let models: Vec<ProviderModelConfig> = serde_json::from_value(result?)?;
                Ok(models)
            }) as BoxFuture<'static, anyhow::Result<Vec<ProviderModelConfig>>>
        }) as RefreshModelsFn
    });
    let images = match value["images"].as_object() {
        Some(map) => {
            let mut out = indexmap::IndexMap::new();
            for (api, implementation) in map {
                if let Some(h) = bridge.handle(implementation) {
                    let images = Arc::new(super::native::JsImages::new(bridge, h.clone())) as Arc<dyn pi_ai::models::ProviderImages>;
                    bridge.origins.record(&images, h);
                    out.insert(api.clone(), images);
                }
            }
            Some(out)
        }
        None => None,
    };
    let classifiers = match value["classifiers"].as_object() {
        Some(map) => {
            let mut out = indexmap::IndexMap::new();
            for (api, implementation) in map {
                if let Some(h) = bridge.handle(implementation) {
                    let classifier =
                        Arc::new(super::native::JsClassifier::new(bridge, h.clone())) as Arc<dyn pi_ai::models::ProviderClassifier>;
                    bridge.origins.record(&classifier, h);
                    out.insert(api.clone(), classifier);
                }
            }
            Some(out)
        }
        None => None,
    };
    if let (Some(f), Some(h)) = (&refresh_models, refresh_handle) {
        bridge.origins.record(f, h);
    }
    let stream_simple = bridge.handle(&value["streamSimple"]).map(|h| {
        let f = js_stream_simple(bridge.clone(), h.clone());
        bridge.origins.record(&f, h);
        f
    });
    Ok(ProviderConfig {
        name: value["name"].as_str().map(str::to_string),
        base_url: value["baseUrl"].as_str().map(str::to_string),
        api_key: value["apiKey"].as_str().map(str::to_string),
        api: value["api"].as_str().map(str::to_string),
        stream_simple,
        images,
        classifiers,
        headers: opt(&value["headers"])?,
        auth_header: value["authHeader"].as_bool(),
        models,
        refresh_models,
        oauth: if value["oauth"].is_object() { js_oauth(bridge, &value["oauth"]) } else { None },
    })
}

fn route_request_json(bridge: &Arc<Bridge>, request: &ModelRouteRequest) -> (Value, Option<super::SignalGuard>) {
    let (signal, guard) = match &request.signal {
        Some(token) => {
            let (v, g) = bridge.signal_for(token);
            (v, Some(g))
        }
        None => (Value::Null, None),
    };
    let mut out = json!({
        "model": serde_json::to_value(&request.model).unwrap_or(Value::Null),
        "thinkingLevel": serde_json::to_value(request.thinking_level).unwrap_or(Value::Null),
        "reason": request.reason.as_str(),
        "messages": serde_json::to_value(&request.messages).unwrap_or(Value::Null),
    });
    if let Some(p) = &request.previous {
        out["previous"] = serde_json::to_value(p).unwrap_or(Value::Null);
    }
    if let Some(f) = &request.failed {
        out["failed"] = serde_json::to_value(f).unwrap_or(Value::Null);
    }
    if let Some(s) = &request.state {
        out["state"] = s.clone();
    }
    if !signal.is_null() {
        out["$signal"] = signal;
    }
    (out, guard)
}

/// `ExtensionVirtualModel` from the JS object of `pi.registerVirtualModel()`.
pub(crate) fn js_virtual_model(bridge: &Arc<Bridge>, value: &Value) -> anyhow::Result<ExtensionVirtualModel> {
    let route = bridge.handle(&value["route"]).ok_or_else(|| anyhow::anyhow!("registerVirtualModel() requires route()"))?;
    let b = bridge.clone();
    Ok(ExtensionVirtualModel {
        provider: str_arg(value, "provider"),
        id: str_arg(value, "id"),
        name: value["name"].as_str().map(str::to_string).unwrap_or_else(|| str_arg(value, "id")),
        thinking_levels: opt(&value["thinkingLevels"])?,
        context_window: value["contextWindow"].as_u64(),
        max_tokens: value["maxTokens"].as_u64(),
        input: opt(&value["input"])?,
        route: Arc::new(move |request: ModelRouteRequest, ctx: ExtensionContext| {
            let b = b.clone();
            let route = route.clone();
            Box::pin(async move {
                let (request_json, _guard) = route_request_json(&b, &request);
                let ctx_id = b.register_ctx(CtxEntry::Base(ctx));
                let result = b.glue_call("invokeRoute", vec![route.to_value(), request_json, json!(ctx_id)]).await?;
                serde_json::from_value::<ModelRoute>(result).map_err(|e| anyhow::anyhow!("Invalid model route: {e}"))
            })
        }),
    })
}

/// `VirtualModelDefinition` from the JS object of `ctx.modelRegistry.registerVirtualModel()`
/// (`route(request)` gets no context, unlike `pi.registerVirtualModel()`).
pub(crate) fn js_virtual_model_definition(
    bridge: &Arc<Bridge>,
    value: &Value,
) -> anyhow::Result<crate::core::virtual_models::VirtualModelDefinition> {
    let route = bridge.handle(&value["route"]).ok_or_else(|| anyhow::anyhow!("registerVirtualModel() requires route()"))?;
    let b = bridge.clone();
    Ok(crate::core::virtual_models::VirtualModelDefinition {
        provider: str_arg(value, "provider"),
        id: str_arg(value, "id"),
        name: str_arg(value, "name"),
        thinking_levels: opt(&value["thinkingLevels"])?,
        context_window: value["contextWindow"].as_u64(),
        max_tokens: value["maxTokens"].as_u64(),
        input: opt(&value["input"])?,
        route: Arc::new(move |request: ModelRouteRequest| {
            let b = b.clone();
            let route = route.clone();
            Box::pin(async move {
                let (request_json, _guard) = route_request_json(&b, &request);
                let result = b.glue_call("invokeRoute", vec![route.to_value(), request_json, Value::Null]).await?;
                serde_json::from_value::<ModelRoute>(result).map_err(|e| anyhow::anyhow!("Invalid model route: {e}"))
            })
        }),
    })
}

fn messages_arg(value: &Value) -> Vec<pi_ai::types::Message> {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

pub(crate) fn dispatch(_bridge: &Arc<Bridge>, _op: &str, payload: Value) -> anyhow::Result<Value> {
    let f = str_arg(&payload, "fn");
    match f.as_str() {
        "telemetry.observe" => return telemetry_observe(&payload["args"][0]),
        "telemetry.event" => return telemetry_event(&payload["args"][0]),
        "telemetry.result" => return telemetry_result(&payload["args"][0]),
        _ => {}
    }
    let args = payload.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
    let a = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
    let s = |i: usize| a(i).as_str().unwrap_or_default().to_string();
    use pi_ai::utils::transcript;
    Ok(match f.as_str() {
        "getModel" => serde_json::to_value(pi_ai::providers::all::get_builtin_model(&s(0), &s(1)))?,
        "getModels" => serde_json::to_value(pi_ai::providers::all::get_builtin_models(&s(0)))?,
        "getProviders" => json!(pi_ai::providers::all::get_builtin_providers()),
        "getEnvApiKey" => {
            let env: Option<pi_ai::types::ProviderEnv> = opt(&a(1))?;
            json!(pi_ai::env_api_keys::get_env_api_key(&s(0), env.as_ref()))
        }
        "findEnvKeys" => {
            let env: Option<pi_ai::types::ProviderEnv> = opt(&a(1))?;
            json!(pi_ai::env_api_keys::find_env_keys(&s(0), env.as_ref()))
        }
        "version" => json!(pi_ai::utils::version::PI_AI_VERSION),
        "formatUserAgent" => {
            let identity: Option<pi_ai::utils::identity::ClientIdentity> = opt(&a(0))?;
            json!(pi_ai::utils::identity::format_user_agent(identity.as_ref()))
        }
        "partialParse" => match pi_ai::utils::json_parse::partial_parse(a(0).as_str().unwrap_or_default()) {
            Ok(v) => v,
            Err(e) => json!({ "$error": e.to_string() }),
        },
        "getBuiltinImageModel" => serde_json::to_value(pi_ai::providers::all::get_builtin_image_model(&s(0), &s(1)))?,
        "getBuiltinClassifierModel" => serde_json::to_value(pi_ai::providers::all::get_builtin_classifier_model(&s(0), &s(1)))?,
        "getBuiltinImageModels" => serde_json::to_value(pi_ai::providers::all::get_builtin_image_models(&s(0)))?,
        "getBuiltinClassifierModels" => serde_json::to_value(pi_ai::providers::all::get_builtin_classifier_models(&s(0)))?,
        "getAllBuiltinModels" => serde_json::to_value(pi_ai::providers::all::get_all_builtin_models(&s(0)))?,
        "getBuiltinModelDataGeneratedAt" => json!(pi_ai::providers::all::get_builtin_model_data_generated_at()),
        "getImageModel" => serde_json::to_value(pi_ai::image_models::get_image_model(&s(0), &s(1)))?,
        "getImageProviders" => json!(pi_ai::image_models::get_image_providers()),
        "getImageModels" => serde_json::to_value(pi_ai::image_models::get_image_models(&s(0)))?,
        "getCurrentSystemMessage" => serde_json::to_value(transcript::get_current_system_message(&messages_arg(&a(0))))?,
        "getCurrentSystemPrompt" => json!(transcript::get_current_system_prompt(&messages_arg(&a(0)))),
        "getCurrentTools" => serde_json::to_value(transcript::get_current_tools(&messages_arg(&a(0))))?,
        "collapseSystemMessages" => {
            let context = pi_ai::utils::transcript::normalize_context(&pi_ai::types::Context {
                system_prompt: None,
                messages: messages_arg(&a(0)),
                tools: None,
            });
            serde_json::to_value(transcript::collapse_system_messages(&context).messages)?
        }
        "contentText" => json!(pi_ai::utils::text::content_text(&serde_json::from_value(a(0))?, "\n")),
        "getSystemMessageText" => json!(pi_ai::utils::text::get_system_message_text(&serde_json::from_value(a(0))?)),
        "isContextOverflow" => json!(pi_ai::utils::overflow::is_context_overflow(&serde_json::from_value(a(0))?, a(1).as_u64())),
        "parseStreamingJson" => pi_ai::utils::json_parse::parse_streaming_json(a(0).as_str()),
        "validateToolCall" => {
            let tools: Vec<pi_ai::types::Tool> = serde_json::from_value(a(0))?;
            let call: pi_ai::types::ToolCall = serde_json::from_value(a(1))?;
            pi_ai::utils::validation::validate_tool_call(&tools, &call).map_err(|e| anyhow::anyhow!("{e}"))?
        }
        "validateToolArguments" => {
            let tool: pi_ai::types::Tool = serde_json::from_value(a(0))?;
            let call: pi_ai::types::ToolCall = serde_json::from_value(a(1))?;
            pi_ai::utils::validation::validate_tool_arguments(&tool, &call).map_err(|e| anyhow::anyhow!("{e}"))?
        }
        _ => return Err(anyhow::anyhow!("pi-ai {f} is not available to extensions in pi-rs")),
    })
}

pub(crate) async fn dispatch_async(bridge: Arc<Bridge>, op: &str, payload: Value) -> anyhow::Result<Value> {
    match op {
        "ai.stream" => {
            let model: Model = serde_json::from_value(payload["model"].clone())?;
            let context: pi_ai::types::Context = serde_json::from_value(payload["context"].clone())?;
            let options = stream_options_from_json(&bridge, &payload["options"]);
            let stream = if payload["simple"].as_bool().unwrap_or(false) {
                pi_ai::compat::stream_simple(&model, &context, options)
            } else {
                pi_ai::compat::stream(&model, &context, options)
            };
            Ok(json!(register_stream(&bridge, stream)))
        }
        "stream.next" => {
            let id = payload["id"].as_u64().unwrap_or_default();
            let Some(rx) = bridge.streams.get(id) else { return Ok(Value::Null) };
            let next = rx.lock().await.recv().await;
            match next {
                Some(event) => Ok(event),
                None => {
                    bridge.streams.remove(id);
                    Ok(Value::Null)
                }
            }
        }
        _ => Err(anyhow::anyhow!("unknown async host call {op}")),
    }
}
