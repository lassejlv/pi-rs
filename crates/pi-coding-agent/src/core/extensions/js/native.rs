//! Native pi-ai objects across the bridge.
//!
//! JavaScript → Rust: provider objects (`pi.registerProvider(provider)`), API implementations
//! (`registerApiProvider`, `createProvider({ api })`), image and classifier implementations, and
//! their auth methods become Rust trait objects ([`JsProvider`], [`JsStreams`], [`JsImages`],
//! [`JsClassifier`], [`JsApiKeyAuth`], [`JsOAuthAuth`]) that call into JavaScript.
//!
//! Rust → JavaScript: Rust providers (`builtinProviders()`), the built-in API implementations
//! (`anthropicMessagesApi()`, ...), auth contexts and login interactions are kept in the bridge's
//! [`Native`] registry and exposed to JavaScript as proxies (`aip.*` host calls, `pi-ai/rust.js`).

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use pi_ai::auth::types::{
    ApiKeyAuth, ApiKeyCredential, ApiKeyResolveInput, AuthCheck, AuthContext, AuthEvent, AuthInfoLink, AuthInteraction,
    AuthPrompt, AuthPromptKind, AuthResult, AuthSelectOption, AuthType, Credential, LoginOptions, ModelAuth, OAuthAuth,
    OAuthCredential, ProviderAuth, ProviderAuthInteraction,
};
use pi_ai::models::{ModelsPublication, Provider, ProviderClassifier, ProviderImages, ProviderStreams, RefreshModelsContext};
use pi_ai::models_store::ModelsStoreEntry;
use pi_ai::types::{
    AnyModel, AssistantImages, AssistantMessageEventStream, ClassifierContext, ClassifierModel, ClassifierResult,
    DeferredHandle, ImageModel, ImagesContext, Model, ProviderHeaders, ProviderResponse, StreamOptions, TranscriptContext,
};
use pi_ai::utils::model_operations::{classifier_error_result, image_error_result};
use pi_jsrt::JsHandle;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::api::{opt, str_arg};
use super::provider::{register_stream, stream_options_from_json, stream_options_to_json};
use super::{AsyncRustCallback, Bridge, SignalGuard, tokio_spawn};
use crate::core::extensions::types::{ProviderConfig, ProviderOAuthConfig, RefreshModelsFn, StreamSimpleFn};

/// A Rust object kept for JavaScript proxies.
#[derive(Clone)]
pub(crate) enum Native {
    Provider(Arc<dyn Provider>),
    Streams(Arc<dyn ProviderStreams>),
    AuthContext(Arc<dyn AuthContext>),
    Interaction(ProviderAuthInteraction),
    /// Parts of a registered provider config that did not come from this runtime.
    StreamSimple(StreamSimpleFn),
    Refresh(RefreshModelsFn),
    Images(Arc<dyn ProviderImages>),
    Classifier(Arc<dyn ProviderClassifier>),
    OAuth(ProviderOAuthConfig),
}

fn js_error_message(err: &anyhow::Error) -> String {
    crate::core::extensions::runner::error_parts(err).0
}

// ---------------------------------------------------------------------------
// Stream options
// ---------------------------------------------------------------------------

/// Resources that live as long as a JavaScript call that received Rust stream options.
#[derive(Default)]
pub(crate) struct OptionGuards {
    signals: Vec<SignalGuard>,
    callbacks: Vec<u64>,
    telemetry: Vec<u64>,
    bridge: Option<Arc<Bridge>>,
}

impl Drop for OptionGuards {
    fn drop(&mut self) {
        if let Some(bridge) = &self.bridge {
            for id in self.callbacks.drain(..) {
                bridge.async_callbacks.remove(id);
            }
        }
        for id in self.telemetry.drain(..) {
            super::provider::release_telemetry_parent(id);
        }
    }
}

/// `StreamOptions` as a JavaScript options object: data fields as JSON, the signal and the
/// `onPayload` / `onResponse` / `onProviderStreamEvent` callbacks as markers that
/// `streamOptionsFromRust` (bridge.js) turns into an `AbortSignal` and functions.
pub(crate) fn options_to_js(bridge: &Arc<Bridge>, options: &StreamOptions) -> (Value, OptionGuards) {
    let mut value = stream_options_to_json(options);
    let mut guards = OptionGuards::default();
    guards.bridge = Some(bridge.clone());
    let map = value.as_object_mut().expect("options object");
    if let Some(identity) = &options.identity {
        map.insert("identity".into(), serde_json::to_value(identity).unwrap_or(Value::Null));
    }
    if let Some(deferred) = &options.deferred {
        map.insert("deferred".into(), serde_json::to_value(deferred).unwrap_or(Value::Null));
    }
    if let Some(ms) = options.websocket_connect_timeout_ms {
        map.insert("websocketConnectTimeoutMs".into(), json!(ms));
    }
    if let Some(token) = &options.signal {
        let (signal, guard) = bridge.signal_for(token);
        map.insert("$signal".into(), signal);
        guards.signals.push(guard);
    }
    if let Some(span) = options.telemetry.clone() {
        let id = super::provider::register_telemetry_parent(span);
        map.insert("telemetry".into(), json!({ "__piTelemetry": id }));
        guards.telemetry.push(id);
    }
    if let Some(on_payload) = options.on_payload.clone() {
        let id = bridge.async_callbacks.insert(Arc::new(move |args: Vec<Value>| {
            let on_payload = on_payload.clone();
            Box::pin(async move {
                let mut args = args.into_iter();
                let payload = args.next().unwrap_or(Value::Null);
                let model: Model = serde_json::from_value(args.next().unwrap_or(Value::Null))?;
                Ok(on_payload(payload, model).await.unwrap_or(Value::Null))
            }) as BoxFuture<'static, anyhow::Result<Value>>
        }) as AsyncRustCallback);
        map.insert("$onPayload".into(), json!(id));
        guards.callbacks.push(id);
    }
    if let Some(on_response) = options.on_response.clone() {
        let id = bridge.async_callbacks.insert(Arc::new(move |args: Vec<Value>| {
            let on_response = on_response.clone();
            Box::pin(async move {
                let mut args = args.into_iter();
                let response = args.next().unwrap_or(Value::Null);
                let model: Model = serde_json::from_value(args.next().unwrap_or(Value::Null))?;
                let response = ProviderResponse {
                    status: response["status"].as_u64().unwrap_or(0) as u16,
                    headers: serde_json::from_value(response["headers"].clone()).unwrap_or_default(),
                };
                on_response(response, model).await;
                Ok(Value::Null)
            }) as BoxFuture<'static, anyhow::Result<Value>>
        }) as AsyncRustCallback);
        map.insert("$onResponse".into(), json!(id));
        guards.callbacks.push(id);
    }
    if let Some(on_event) = options.on_provider_stream_event.clone() {
        let id = bridge.async_callbacks.insert(Arc::new(move |args: Vec<Value>| {
            let on_event = on_event.clone();
            Box::pin(async move {
                let mut args = args.into_iter();
                let data = args.next().unwrap_or(Value::Null);
                let model: Model = serde_json::from_value(args.next().unwrap_or(Value::Null))?;
                on_event(data, model).await;
                Ok(Value::Null)
            }) as BoxFuture<'static, anyhow::Result<Value>>
        }) as AsyncRustCallback);
        map.insert("$onProviderStreamEvent".into(), json!(id));
        guards.callbacks.push(id);
    }
    (value, guards)
}

/// JavaScript stream options (functions as handle markers) as `StreamOptions`.
pub(crate) fn options_from_js(bridge: &Arc<Bridge>, value: &Value) -> StreamOptions {
    let mut data = value.clone();
    let mut handles = Map::new();
    if let Some(map) = data.as_object_mut() {
        for key in ["onPayload", "onResponse", "onProviderStreamEvent", "identity", "deferred", "websocketConnectTimeoutMs", "fetch"] {
            if let Some(v) = map.remove(key) {
                handles.insert(key.to_string(), v);
            }
        }
    }
    let mut options = stream_options_from_json(bridge, &data);
    options.identity = handles.get("identity").and_then(|v| serde_json::from_value(v.clone()).ok());
    options.deferred = handles.get("deferred").and_then(|v| serde_json::from_value(v.clone()).ok());
    options.websocket_connect_timeout_ms = handles.get("websocketConnectTimeoutMs").and_then(Value::as_u64);
    if let Some(h) = handles.get("onPayload").and_then(|v| bridge.handle(v)) {
        let b = bridge.clone();
        options.on_payload = Some(Arc::new(move |payload: Value, model: Model| {
            let b = b.clone();
            let h = h.clone();
            Box::pin(async move {
                let model = serde_json::to_value(&model).unwrap_or(Value::Null);
                let result = b.glue_call("callFunctionAsync", vec![h.to_value(), json!([payload, model])]).await.ok()?;
                if result.is_null() { None } else { Some(result) }
            }) as BoxFuture<'static, Option<Value>>
        }));
    }
    if let Some(h) = handles.get("onResponse").and_then(|v| bridge.handle(v)) {
        let b = bridge.clone();
        options.on_response = Some(Arc::new(move |response: ProviderResponse, model: Model| {
            let b = b.clone();
            let h = h.clone();
            Box::pin(async move {
                let response = json!({ "status": response.status, "headers": response.headers });
                let model = serde_json::to_value(&model).unwrap_or(Value::Null);
                let _ = b.glue_call("callFunctionAsync", vec![h.to_value(), json!([response, model])]).await;
            }) as BoxFuture<'static, ()>
        }));
    }
    if let Some(h) = handles.get("onProviderStreamEvent").and_then(|v| bridge.handle(v)) {
        let b = bridge.clone();
        options.on_provider_stream_event = Some(Arc::new(move |data: Value, model: Model| {
            let b = b.clone();
            let h = h.clone();
            Box::pin(async move {
                let model = serde_json::to_value(&model).unwrap_or(Value::Null);
                let _ = b.glue_call("callFunctionAsync", vec![h.to_value(), json!([data, model])]).await;
            }) as BoxFuture<'static, ()>
        }));
    }
    options
}

// ---------------------------------------------------------------------------
// JavaScript streams as Rust streams
// ---------------------------------------------------------------------------

fn error_message(model: &Model, message: String) -> pi_ai::types::AssistantMessage {
    pi_ai::types::AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason: pi_ai::types::StopReason::Error,
        error_message: Some(message),
        timestamp: chrono::Utc::now().timestamp_millis(),
        ..Default::default()
    }
}

/// Call a JavaScript stream function (`target` itself when `method` is `None`, else
/// `target[method]`) with `(model, ...args, options)` and forward the resulting
/// `AssistantMessageEventStream` into a Rust stream.
pub(crate) fn js_stream(
    bridge: Arc<Bridge>,
    target: JsHandle,
    method: Option<&'static str>,
    model: Model,
    args: Vec<Value>,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    let (sender, stream) = AssistantMessageEventStream::new();
    tokio_spawn(async move {
        let (options_value, guards) = options_to_js(&bridge, &options);
        let sender2 = sender.clone();
        let push = bridge.register_callback(Arc::new(move |args: Vec<Value>| {
            let event = args.into_iter().next().unwrap_or(Value::Null);
            match serde_json::from_value(event) {
                Ok(event) => sender2.push(event),
                Err(e) => tracing::warn!("invalid assistant message event from an extension stream: {e}"),
            }
            Ok(Value::Null)
        }));
        let result = async {
            let mut call_args = vec![serde_json::to_value(&model)?];
            call_args.extend(args);
            let stream_ref = bridge
                .glue_call("invokeStream", vec![target.to_value(), json!(method), Value::Array(call_args), options_value])
                .await?;
            bridge.glue_call("pumpStream", vec![stream_ref, json!(push)]).await
        }
        .await;
        bridge.callbacks.remove(push);
        drop(guards);
        if let Err(err) = result {
            sender.push(pi_ai::types::AssistantMessageEvent::Error {
                reason: pi_ai::types::StopReason::Error,
                error: error_message(&model, js_error_message(&err)),
            });
        }
        sender.end(None);
    });
    stream
}

/// Call `target[method](...args, options)` and await its result.
async fn js_call_with_options(
    bridge: &Arc<Bridge>,
    target: &JsHandle,
    method: &str,
    args: Vec<Value>,
    options: &StreamOptions,
) -> anyhow::Result<Value> {
    let (options_value, guards) = options_to_js(bridge, options);
    let mut call_args = args;
    call_args.push(options_value);
    let result = bridge.glue_call("invokeWithRustOptions", vec![target.to_value(), json!(method), Value::Array(call_args)]).await;
    drop(guards);
    result
}

/// A JavaScript `ProviderStreams` object (`{ stream, streamSimple, fetchDeferred?, cancelDeferred? }`).
pub(crate) struct JsStreams {
    bridge: Arc<Bridge>,
    handle: JsHandle,
    fetch_deferred: bool,
    cancel_deferred: bool,
}

impl JsStreams {
    pub(crate) fn new(bridge: &Arc<Bridge>, handle: JsHandle, desc: &Value) -> Self {
        Self {
            bridge: bridge.clone(),
            handle,
            fetch_deferred: desc["fetchDeferred"].as_bool().unwrap_or(false),
            cancel_deferred: desc["cancelDeferred"].as_bool().unwrap_or(false),
        }
    }
}

impl ProviderStreams for JsStreams {
    fn stream(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        let context = serde_json::to_value(context).unwrap_or(Value::Null);
        js_stream(self.bridge.clone(), self.handle.clone(), Some("stream"), model.clone(), vec![context], options)
    }

    fn stream_simple(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        let context = serde_json::to_value(context).unwrap_or(Value::Null);
        js_stream(self.bridge.clone(), self.handle.clone(), Some("streamSimple"), model.clone(), vec![context], options)
    }

    fn supports_fetch_deferred(&self) -> bool {
        self.fetch_deferred
    }

    fn fetch_deferred(&self, model: &Model, handle: &DeferredHandle, options: StreamOptions) -> AssistantMessageEventStream {
        let handle = serde_json::to_value(handle).unwrap_or(Value::Null);
        js_stream(self.bridge.clone(), self.handle.clone(), Some("fetchDeferred"), model.clone(), vec![handle], options)
    }

    fn supports_cancel_deferred(&self) -> bool {
        self.cancel_deferred
    }

    fn cancel_deferred(&self, model: &Model, handle: &DeferredHandle, options: StreamOptions) -> BoxFuture<'static, anyhow::Result<()>> {
        let bridge = self.bridge.clone();
        let target = self.handle.clone();
        let args = vec![serde_json::to_value(model).unwrap_or(Value::Null), serde_json::to_value(handle).unwrap_or(Value::Null)];
        Box::pin(async move {
            js_call_with_options(&bridge, &target, "cancelDeferred", args, &options).await?;
            Ok(())
        })
    }
}

/// A JavaScript `ProviderImages` object (`{ generateImages }`).
pub(crate) struct JsImages {
    bridge: Arc<Bridge>,
    handle: JsHandle,
}

impl JsImages {
    pub(crate) fn new(bridge: &Arc<Bridge>, handle: JsHandle) -> Self {
        Self { bridge: bridge.clone(), handle }
    }
}

#[async_trait]
impl ProviderImages for JsImages {
    async fn generate_images(&self, model: &ImageModel, context: &ImagesContext, options: StreamOptions) -> AssistantImages {
        let args = vec![serde_json::to_value(model).unwrap_or(Value::Null), serde_json::to_value(context).unwrap_or(Value::Null)];
        match js_call_with_options(&self.bridge, &self.handle, "generateImages", args, &options).await {
            Ok(value) => serde_json::from_value(value).unwrap_or_else(|e| image_error_result(model, &e, false)),
            Err(e) => image_error_result(model, &js_error_message(&e), options.is_aborted()),
        }
    }
}

/// A JavaScript `ProviderClassifier` object (`{ classify }`).
pub(crate) struct JsClassifier {
    bridge: Arc<Bridge>,
    handle: JsHandle,
}

impl JsClassifier {
    pub(crate) fn new(bridge: &Arc<Bridge>, handle: JsHandle) -> Self {
        Self { bridge: bridge.clone(), handle }
    }
}

#[async_trait]
impl ProviderClassifier for JsClassifier {
    async fn classify(&self, model: &ClassifierModel, context: &ClassifierContext, options: StreamOptions) -> ClassifierResult {
        let args = vec![serde_json::to_value(model).unwrap_or(Value::Null), serde_json::to_value(context).unwrap_or(Value::Null)];
        match js_call_with_options(&self.bridge, &self.handle, "classify", args, &options).await {
            Ok(value) => serde_json::from_value(value).unwrap_or_else(|e| classifier_error_result(model, &e, false)),
            Err(e) => classifier_error_result(model, &js_error_message(&e), options.is_aborted()),
        }
    }
}

// ---------------------------------------------------------------------------
// Auth values
// ---------------------------------------------------------------------------

fn model_auth_from_js(value: &Value) -> ModelAuth {
    ModelAuth {
        api_key: value["apiKey"].as_str().map(str::to_string),
        headers: opt::<ProviderHeaders>(&value["headers"]).ok().flatten(),
        base_url: value["baseUrl"].as_str().map(str::to_string),
    }
}

fn model_auth_to_js(auth: &ModelAuth) -> Value {
    let mut out = Map::new();
    if let Some(k) = &auth.api_key {
        out.insert("apiKey".into(), json!(k));
    }
    if let Some(h) = &auth.headers {
        out.insert("headers".into(), serde_json::to_value(h).unwrap_or(Value::Null));
    }
    if let Some(u) = &auth.base_url {
        out.insert("baseUrl".into(), json!(u));
    }
    Value::Object(out)
}

fn auth_result_from_js(value: &Value) -> Option<AuthResult> {
    if !value.is_object() {
        return None;
    }
    Some(AuthResult {
        auth: model_auth_from_js(&value["auth"]),
        env: opt(&value["env"]).ok().flatten(),
        source: value["source"].as_str().map(str::to_string),
    })
}

pub(crate) fn auth_result_to_js(result: &AuthResult) -> Value {
    let mut out = Map::new();
    out.insert("auth".into(), model_auth_to_js(&result.auth));
    if let Some(env) = &result.env {
        out.insert("env".into(), serde_json::to_value(env).unwrap_or(Value::Null));
    }
    if let Some(s) = &result.source {
        out.insert("source".into(), json!(s));
    }
    Value::Object(out)
}

fn auth_check_from_js(value: &Value) -> Option<AuthCheck> {
    if !value.is_object() {
        return None;
    }
    Some(AuthCheck {
        source: value["source"].as_str().map(str::to_string),
        auth_type: if value["type"] == "oauth" { AuthType::OAuth } else { AuthType::ApiKey },
    })
}

pub(crate) fn auth_check_to_js(check: &AuthCheck) -> Value {
    let mut out = json!({ "type": check.auth_type.as_str() });
    if let Some(s) = &check.source {
        out["source"] = json!(s);
    }
    out
}

fn api_key_credential_from_js(value: &Value) -> ApiKeyCredential {
    ApiKeyCredential { key: value["key"].as_str().map(str::to_string), env: opt(&value["env"]).ok().flatten() }
}

fn api_key_credential_to_js(credential: &ApiKeyCredential) -> Value {
    serde_json::to_value(Credential::ApiKey(credential.clone())).unwrap_or(Value::Null)
}

fn oauth_credential_to_js(credential: &OAuthCredential) -> Value {
    serde_json::to_value(Credential::OAuth(credential.clone())).unwrap_or(Value::Null)
}

fn oauth_credential_from_js(value: &Value) -> anyhow::Result<OAuthCredential> {
    let mut value = value.clone();
    if let Some(map) = value.as_object_mut() {
        map.remove("type");
    }
    serde_json::from_value(value).map_err(|e| anyhow::anyhow!("Invalid OAuth credential: {e}"))
}

pub(crate) fn auth_prompt_to_js(bridge: &Arc<Bridge>, prompt: &AuthPrompt) -> (Value, Option<SignalGuard>) {
    let kind = match prompt.kind {
        AuthPromptKind::Text => "text",
        AuthPromptKind::Secret => "secret",
        AuthPromptKind::Select => "select",
        AuthPromptKind::ManualCode => "manual_code",
    };
    let mut out = json!({ "type": kind, "message": prompt.message });
    if let Some(p) = &prompt.placeholder {
        out["placeholder"] = json!(p);
    }
    if prompt.kind == AuthPromptKind::Select {
        out["options"] = serde_json::to_value(&prompt.options).unwrap_or(Value::Null);
    }
    let guard = prompt.signal.as_ref().map(|token| {
        let (value, guard) = bridge.signal_for(token);
        out["$signal"] = value;
        guard
    });
    (out, guard)
}

fn auth_prompt_from_js(bridge: &Bridge, value: &Value) -> AuthPrompt {
    let kind = match value["type"].as_str() {
        Some("secret") => AuthPromptKind::Secret,
        Some("select") => AuthPromptKind::Select,
        Some("manual_code") => AuthPromptKind::ManualCode,
        _ => AuthPromptKind::Text,
    };
    AuthPrompt {
        kind,
        message: str_arg(value, "message"),
        placeholder: value["placeholder"].as_str().map(str::to_string),
        options: opt::<Vec<AuthSelectOption>>(&value["options"]).ok().flatten().unwrap_or_default(),
        signal: bridge.token(&value["signal"]),
    }
}

pub(crate) fn auth_event_to_js(event: &AuthEvent) -> Value {
    match event {
        AuthEvent::Info { message, links } => {
            let links: Vec<Value> = links
                .iter()
                .map(|l| match &l.label {
                    Some(label) => json!({ "url": l.url, "label": label }),
                    None => json!({ "url": l.url }),
                })
                .collect();
            let mut out = json!({ "type": "info", "message": message });
            if !links.is_empty() {
                out["links"] = Value::Array(links);
            }
            out
        }
        AuthEvent::AuthUrl { url, instructions } => {
            let mut out = json!({ "type": "auth_url", "url": url });
            if let Some(i) = instructions {
                out["instructions"] = json!(i);
            }
            out
        }
        AuthEvent::DeviceCode { user_code, verification_uri, interval_seconds, expires_in_seconds } => {
            let mut out = json!({ "type": "device_code", "userCode": user_code, "verificationUri": verification_uri });
            if let Some(i) = interval_seconds {
                out["intervalSeconds"] = json!(i);
            }
            if let Some(e) = expires_in_seconds {
                out["expiresInSeconds"] = json!(e);
            }
            out
        }
        AuthEvent::Progress { message } => json!({ "type": "progress", "message": message }),
    }
}

fn auth_event_from_js(value: &Value) -> AuthEvent {
    match value["type"].as_str() {
        Some("auth_url") => AuthEvent::AuthUrl {
            url: str_arg(value, "url"),
            instructions: value["instructions"].as_str().map(str::to_string),
        },
        Some("device_code") => AuthEvent::DeviceCode {
            user_code: str_arg(value, "userCode"),
            verification_uri: str_arg(value, "verificationUri"),
            interval_seconds: value["intervalSeconds"].as_u64(),
            expires_in_seconds: value["expiresInSeconds"].as_u64(),
        },
        Some("progress") => AuthEvent::Progress { message: str_arg(value, "message") },
        _ => AuthEvent::Info {
            message: str_arg(value, "message"),
            links: value["links"]
                .as_array()
                .map(|links| {
                    links
                        .iter()
                        .map(|l| AuthInfoLink { url: str_arg(l, "url"), label: l["label"].as_str().map(str::to_string) })
                        .collect()
                })
                .unwrap_or_default(),
        },
    }
}

/// A login interaction for JavaScript: `{ native }` (bridge.js builds the proxy) plus its signal.
fn interaction_to_js(bridge: &Arc<Bridge>, interaction: &ProviderAuthInteraction) -> (Value, SignalGuard) {
    let id = bridge.natives.insert(Native::Interaction(interaction.clone()));
    let (signal, guard) = bridge.signal_for(&interaction.signal);
    (json!({ "$interaction": id, "$signal": signal }), guard)
}

fn auth_context_to_js(bridge: &Arc<Bridge>, ctx: &Arc<dyn AuthContext>) -> Value {
    json!({ "$authContext": bridge.natives.insert(Native::AuthContext(ctx.clone())) })
}

// ---------------------------------------------------------------------------
// JavaScript auth methods as Rust auth methods
// ---------------------------------------------------------------------------

/// A JavaScript `ApiKeyAuth` object.
pub(crate) struct JsApiKeyAuth {
    bridge: Arc<Bridge>,
    handle: JsHandle,
    name: String,
    login: bool,
    check: bool,
}

impl JsApiKeyAuth {
    async fn call(&self, method: &str, payload: Value) -> anyhow::Result<Value> {
        self.bridge.glue_call("invokeAuthMethod", vec![self.handle.to_value(), json!(method), payload]).await
    }

    fn resolve_payload(&self, input: &ApiKeyResolveInput) -> (Value, SignalGuard) {
        let (signal, guard) = self.bridge.signal_for(&input.signal);
        let mut payload = json!({ "ctx": auth_context_to_js(&self.bridge, &input.ctx), "$signal": signal });
        if let Some(c) = &input.credential {
            payload["credential"] = api_key_credential_to_js(c);
        }
        (payload, guard)
    }
}

#[async_trait]
impl ApiKeyAuth for JsApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn supports_login(&self) -> bool {
        self.login
    }

    async fn login(&self, interaction: ProviderAuthInteraction) -> anyhow::Result<ApiKeyCredential> {
        let (value, _guard) = interaction_to_js(&self.bridge, &interaction);
        let result = self.call("login", json!({ "interaction": value })).await?;
        Ok(api_key_credential_from_js(&result))
    }

    fn supports_check(&self) -> bool {
        self.check
    }

    async fn check(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthCheck>> {
        let (payload, _guard) = self.resolve_payload(&input);
        Ok(auth_check_from_js(&self.call("check", payload).await?))
    }

    async fn resolve(&self, input: ApiKeyResolveInput) -> anyhow::Result<Option<AuthResult>> {
        let (payload, _guard) = self.resolve_payload(&input);
        Ok(auth_result_from_js(&self.call("resolve", payload).await?))
    }
}

/// A JavaScript `OAuthAuth` object.
pub(crate) struct JsOAuthAuth {
    bridge: Arc<Bridge>,
    handle: JsHandle,
    name: String,
    is_subscription: Option<bool>,
    login_label: Option<String>,
}

#[async_trait]
impl OAuthAuth for JsOAuthAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_subscription(&self) -> Option<bool> {
        self.is_subscription
    }

    fn login_label(&self) -> Option<&str> {
        self.login_label.as_deref()
    }

    async fn login(&self, interaction: ProviderAuthInteraction, options: Option<LoginOptions>) -> anyhow::Result<OAuthCredential> {
        let (value, _guard) = interaction_to_js(&self.bridge, &interaction);
        let mut login_options = Map::new();
        let mut device_cb = None;
        if let Some(options) = &options {
            if let Some(get) = options.get_device_id.clone() {
                let id = self.bridge.register_callback(Arc::new(move |_| Ok(json!(get()))));
                login_options.insert("$getDeviceId".into(), json!(id));
                device_cb = Some(id);
            }
            if let Some(name) = &options.agent_name {
                login_options.insert("agentName".into(), json!(name));
            }
        }
        let payload = json!({ "interaction": value, "options": if options.is_some() { Value::Object(login_options) } else { Value::Null } });
        let result = self.bridge.glue_call("invokeAuthMethod", vec![self.handle.to_value(), json!("login"), payload]).await;
        if let Some(id) = device_cb {
            self.bridge.callbacks.remove(id);
        }
        oauth_credential_from_js(&result?)
    }

    async fn refresh(&self, credential: OAuthCredential, signal: CancellationToken) -> anyhow::Result<OAuthCredential> {
        let (signal_value, _guard) = self.bridge.signal_for(&signal);
        let payload = json!({ "credential": oauth_credential_to_js(&credential), "$signal": signal_value });
        let result = self.bridge.glue_call("invokeAuthMethod", vec![self.handle.to_value(), json!("refresh"), payload]).await?;
        oauth_credential_from_js(&result)
    }

    async fn to_auth(&self, credential: &OAuthCredential) -> anyhow::Result<ModelAuth> {
        let payload = json!({ "credential": oauth_credential_to_js(credential) });
        let result = self.bridge.glue_call("invokeAuthMethod", vec![self.handle.to_value(), json!("toAuth"), payload]).await?;
        Ok(model_auth_from_js(&result))
    }
}

/// `ProviderAuth` from a JavaScript auth description (`describeAuth` in bridge.js).
pub(crate) fn js_provider_auth(bridge: &Arc<Bridge>, desc: &Value) -> ProviderAuth {
    let api_key = bridge.handle(&desc["apiKey"]["object"]).map(|handle| {
        Arc::new(JsApiKeyAuth {
            bridge: bridge.clone(),
            handle,
            name: str_arg(&desc["apiKey"], "name"),
            login: desc["apiKey"]["login"].as_bool().unwrap_or(false),
            check: desc["apiKey"]["check"].as_bool().unwrap_or(false),
        }) as Arc<dyn ApiKeyAuth>
    });
    let oauth = bridge.handle(&desc["oauth"]["object"]).map(|handle| {
        Arc::new(JsOAuthAuth {
            bridge: bridge.clone(),
            handle,
            name: str_arg(&desc["oauth"], "name"),
            is_subscription: desc["oauth"]["isSubscription"].as_bool(),
            login_label: desc["oauth"]["loginLabel"].as_str().map(str::to_string),
        }) as Arc<dyn OAuthAuth>
    });
    ProviderAuth { api_key, oauth }
}

/// A JavaScript `AuthContext` (`{ env, fileExists }`) as a Rust auth context.
struct JsAuthContext {
    bridge: Arc<Bridge>,
    handle: JsHandle,
}

#[async_trait]
impl AuthContext for JsAuthContext {
    async fn env(&self, name: &str) -> Option<String> {
        let value = self.bridge.glue_call("callMethodAsync", vec![self.handle.to_value(), json!("env"), json!([name])]).await.ok()?;
        value.as_str().map(str::to_string)
    }

    async fn file_exists(&self, path: &str) -> bool {
        self.bridge
            .glue_call("callMethodAsync", vec![self.handle.to_value(), json!("fileExists"), json!([path])])
            .await
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }
}

/// A JavaScript login interaction (`{ prompt, notify, signal }`) as a Rust interaction.
struct JsAuthInteraction {
    bridge: Arc<Bridge>,
    handle: JsHandle,
}

#[async_trait]
impl AuthInteraction for JsAuthInteraction {
    async fn prompt(&self, prompt: AuthPrompt) -> anyhow::Result<String> {
        let (value, _guard) = auth_prompt_to_js(&self.bridge, &prompt);
        let result = self.bridge.glue_call("invokeInteractionPrompt", vec![self.handle.to_value(), value]).await?;
        Ok(result.as_str().map(str::to_string).unwrap_or_else(|| result.to_string()))
    }

    fn notify(&self, event: AuthEvent) {
        super::api::post_call(&self.handle, Some("notify"), vec![auth_event_to_js(&event)]);
    }
}

// ---------------------------------------------------------------------------
// JavaScript providers as Rust providers
// ---------------------------------------------------------------------------

/// A JavaScript `Provider` object (`pi.registerProvider(provider)`).
pub(crate) struct JsProvider {
    bridge: Arc<Bridge>,
    handle: JsHandle,
    id: String,
    name: String,
    base_url: Option<String>,
    headers: Option<ProviderHeaders>,
    auth: ProviderAuth,
    has_all_models: bool,
    has_refresh: bool,
    has_filter: bool,
    has_filter_all: bool,
    has_fetch_deferred: bool,
    has_cancel_deferred: bool,
    has_images: bool,
    has_classify: bool,
}

impl JsProvider {
    /// From a provider description (`describeNativeProvider` in bridge.js).
    pub(crate) fn new(bridge: &Arc<Bridge>, desc: &Value) -> anyhow::Result<Self> {
        let handle = bridge.handle(&desc["object"]).ok_or_else(|| anyhow::anyhow!("registerProvider() expects a provider object"))?;
        let flag = |key: &str| desc["methods"][key].as_bool().unwrap_or(false);
        Ok(Self {
            bridge: bridge.clone(),
            handle,
            id: str_arg(desc, "id"),
            name: desc["name"].as_str().map(str::to_string).unwrap_or_else(|| str_arg(desc, "id")),
            base_url: desc["baseUrl"].as_str().map(str::to_string),
            headers: opt(&desc["headers"]).ok().flatten(),
            auth: js_provider_auth(bridge, &desc["auth"]),
            has_all_models: flag("getAllModels"),
            has_refresh: flag("refreshModels"),
            has_filter: flag("filterModels"),
            has_filter_all: flag("filterAllModels"),
            has_fetch_deferred: flag("fetchDeferred"),
            has_cancel_deferred: flag("cancelDeferred"),
            has_images: flag("generateImages"),
            has_classify: flag("classify"),
        })
    }

    fn call_sync(&self, method: &str, args: Vec<Value>) -> Option<Value> {
        self.handle.call_method_blocking(method, args).ok()
    }
}

/// `RefreshModelsContext` as a JavaScript object (publish as an async callback).
pub(crate) fn refresh_context_to_js(bridge: &Arc<Bridge>, ctx: &RefreshModelsContext) -> (Value, SignalGuard, u64) {
    let (signal, guard) = bridge.signal_for(&ctx.signal);
    let publish = ctx.publish.clone();
    let b = bridge.clone();
    let publish_id = bridge.async_callbacks.insert(Arc::new(move |args: Vec<Value>| {
        let publish = publish.clone();
        let b = b.clone();
        Box::pin(async move {
            let publication = args.into_iter().next().unwrap_or(Value::Null);
            let persist = match publication.get("persist") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(entry) => Some(Some(serde_json::from_value::<ModelsStoreEntry>(entry.clone())?)),
            };
            let update = b.handle(&publication["update"]).map(|h| {
                Box::new(move || {
                    let _ = h.call_blocking(vec![]);
                }) as Box<dyn FnOnce() + Send>
            });
            Ok(json!(publish(ModelsPublication { persist, update }).await?))
        }) as BoxFuture<'static, anyhow::Result<Value>>
    }) as AsyncRustCallback);
    let mut value = json!({
        "allowNetwork": ctx.allow_network,
        "$signal": signal,
        "$publish": publish_id,
    });
    if let Some(c) = &ctx.credential {
        value["credential"] = serde_json::to_value(c).unwrap_or(Value::Null);
    }
    if let Some(s) = &ctx.stored {
        value["stored"] = serde_json::to_value(s).unwrap_or(Value::Null);
    }
    if let Some(f) = ctx.force {
        value["force"] = json!(f);
    }
    if let Some(u) = &ctx.base_url {
        value["baseUrl"] = json!(u);
    }
    (value, guard, publish_id)
}

#[async_trait]
impl Provider for JsProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    fn headers(&self) -> Option<&ProviderHeaders> {
        self.headers.as_ref()
    }

    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    fn get_models(&self) -> Vec<Model> {
        self.call_sync("getModels", vec![]).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }

    fn get_all_models(&self) -> Vec<AnyModel> {
        if !self.has_all_models {
            return self.get_models().into_iter().map(AnyModel::Chat).collect();
        }
        self.call_sync("getAllModels", vec![]).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }

    fn supports_refresh(&self) -> bool {
        self.has_refresh
    }

    async fn refresh_models(&self, context: RefreshModelsContext) -> anyhow::Result<()> {
        if !self.has_refresh {
            return Ok(());
        }
        let (value, _guard, publish_id) = refresh_context_to_js(&self.bridge, &context);
        let result = self.bridge.glue_call("invokeRefreshModels", vec![self.handle.to_value(), value]).await;
        self.bridge.async_callbacks.remove(publish_id);
        result.map(|_| ())
    }

    fn filter_models(&self, models: &[Model], credential: Option<&Credential>) -> Option<Vec<Model>> {
        if !self.has_filter {
            return None;
        }
        let args = vec![serde_json::to_value(models).unwrap_or(Value::Null), serde_json::to_value(credential).unwrap_or(Value::Null)];
        self.bridge
            .glue_call_blocking("invokeFilter", vec![self.handle.to_value(), json!("filterModels"), Value::Array(args)])
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
    }

    fn filter_all_models(&self, models: &[AnyModel], credential: Option<&Credential>) -> Option<Vec<AnyModel>> {
        if !self.has_filter_all {
            return None;
        }
        let args = vec![serde_json::to_value(models).unwrap_or(Value::Null), serde_json::to_value(credential).unwrap_or(Value::Null)];
        self.bridge
            .glue_call_blocking("invokeFilter", vec![self.handle.to_value(), json!("filterAllModels"), Value::Array(args)])
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
    }

    fn stream(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        let context = serde_json::to_value(context).unwrap_or(Value::Null);
        js_stream(self.bridge.clone(), self.handle.clone(), Some("stream"), model.clone(), vec![context], options)
    }

    fn stream_simple(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        let context = serde_json::to_value(context).unwrap_or(Value::Null);
        js_stream(self.bridge.clone(), self.handle.clone(), Some("streamSimple"), model.clone(), vec![context], options)
    }

    fn supports_fetch_deferred(&self) -> bool {
        self.has_fetch_deferred
    }

    fn fetch_deferred(&self, model: &Model, handle: &DeferredHandle, options: StreamOptions) -> AssistantMessageEventStream {
        let handle = serde_json::to_value(handle).unwrap_or(Value::Null);
        js_stream(self.bridge.clone(), self.handle.clone(), Some("fetchDeferred"), model.clone(), vec![handle], options)
    }

    fn supports_cancel_deferred(&self) -> bool {
        self.has_cancel_deferred
    }

    async fn cancel_deferred(&self, model: &Model, handle: &DeferredHandle, options: StreamOptions) -> anyhow::Result<()> {
        let args = vec![serde_json::to_value(model)?, serde_json::to_value(handle)?];
        js_call_with_options(&self.bridge, &self.handle, "cancelDeferred", args, &options).await?;
        Ok(())
    }

    fn supports_images(&self) -> bool {
        self.has_images
    }

    async fn generate_images(&self, model: &ImageModel, context: &ImagesContext, options: StreamOptions) -> AssistantImages {
        JsImages { bridge: self.bridge.clone(), handle: self.handle.clone() }.generate_images(model, context, options).await
    }

    fn supports_classify(&self) -> bool {
        self.has_classify
    }

    async fn classify(&self, model: &ClassifierModel, context: &ClassifierContext, options: StreamOptions) -> ClassifierResult {
        JsClassifier { bridge: self.bridge.clone(), handle: self.handle.clone() }.classify(model, context, options).await
    }
}

/// A provider registered with `pi.registerProvider(provider)`: a Rust provider proxy passes
/// the Rust provider itself, any other object becomes a [`JsProvider`].
pub(crate) fn native_provider(bridge: &Arc<Bridge>, desc: &Value) -> anyhow::Result<Arc<dyn Provider>> {
    if let Some(id) = desc["native"].as_u64()
        && let Some(Native::Provider(provider)) = bridge.natives.get(id)
    {
        return Ok(provider);
    }
    let provider = JsProvider::new(bridge, desc)?;
    let handle = provider.handle.clone();
    let provider: Arc<dyn Provider> = Arc::new(provider);
    bridge.origins.record(&provider, handle);
    Ok(provider)
}

/// A Rust provider as a JavaScript value: the extension's own object when it registered the
/// provider from this runtime, otherwise one `RustProvider` per provider (stable identity).
pub(crate) fn provider_to_js(bridge: &Arc<Bridge>, provider: Arc<dyn Provider>) -> Value {
    if let Some(origin) = bridge.origins.get(&provider) {
        return json!({ "object": origin });
    }
    let key = Arc::as_ptr(&provider) as *const () as usize;
    let existing = bridge.native_provider_ids.lock().get(&key).copied();
    if let Some(id) = existing
        && matches!(bridge.natives.get(id), Some(Native::Provider(_)))
    {
        return json!({ "native": id });
    }
    let desc = describe_provider(bridge, provider);
    if let Some(id) = desc["native"].as_u64() {
        bridge.native_provider_ids.lock().insert(key, id);
    }
    json!({ "desc": desc })
}

/// `ProviderStreams` from a description (`describeStreams` in bridge.js): Rust streams behind a
/// proxy, or a JavaScript object.
pub(crate) fn native_streams(bridge: &Arc<Bridge>, desc: &Value) -> Option<Arc<dyn ProviderStreams>> {
    if let Some(id) = desc["native"].as_u64()
        && let Some(Native::Streams(streams)) = bridge.natives.get(id)
    {
        return Some(streams);
    }
    let handle = bridge.handle(&desc["object"])?;
    Some(Arc::new(JsStreams::new(bridge, handle, desc)))
}

// ---------------------------------------------------------------------------
// Rust providers and API implementations as JavaScript objects (`aip.*` host calls)
// ---------------------------------------------------------------------------

/// A transcript context (`{ messages }`) from JavaScript.
fn transcript_from_js(value: &Value) -> anyhow::Result<TranscriptContext> {
    let messages = serde_json::from_value(value["messages"].clone())?;
    Ok(pi_ai::utils::transcript::normalize_context(&pi_ai::types::Context { system_prompt: None, messages, tools: None }))
}

fn provider_of(bridge: &Bridge, payload: &Value) -> anyhow::Result<Arc<dyn Provider>> {
    match bridge.natives.get(payload["id"].as_u64().unwrap_or_default()) {
        Some(Native::Provider(p)) => Ok(p),
        _ => Err(anyhow::anyhow!("unknown provider")),
    }
}

/// Register a Rust provider for JavaScript and describe it (`RustProvider` in pi-ai/rust.js).
pub(crate) fn describe_provider(bridge: &Arc<Bridge>, provider: Arc<dyn Provider>) -> Value {
    let auth = provider.auth();
    let api_key = auth.api_key.as_ref().map(|a| json!({ "name": a.name(), "login": a.supports_login(), "check": a.supports_check() }));
    let oauth = auth.oauth.as_ref().map(|o| {
        let mut out = json!({ "name": o.name() });
        if let Some(s) = o.is_subscription() {
            out["isSubscription"] = json!(s);
        }
        if let Some(l) = o.login_label() {
            out["loginLabel"] = json!(l);
        }
        out
    });
    let mut out = json!({
        "id": provider.id(),
        "name": provider.name(),
        "auth": { "apiKey": api_key, "oauth": oauth },
        "refresh": provider.supports_refresh(),
        // `filter_models` returns `None` when the provider has no filter (TS: `filterModels` undefined).
        "filter": provider.filter_models(&[], None).is_some(),
        "filterAll": provider.filter_all_models(&[], None).is_some(),
        "fetchDeferred": provider.supports_fetch_deferred(),
        "cancelDeferred": provider.supports_cancel_deferred(),
        "images": provider.supports_images(),
        "classify": provider.supports_classify(),
    });
    if let Some(u) = provider.base_url() {
        out["baseUrl"] = json!(u);
    }
    if let Some(h) = provider.headers() {
        out["headers"] = serde_json::to_value(h).unwrap_or(Value::Null);
    }
    out["native"] = json!(bridge.natives.insert(Native::Provider(provider)));
    out
}

// ---------------------------------------------------------------------------
// Registered provider configs as JavaScript values (`ctx.modelRegistry.getRegisteredProviderConfig`)
// ---------------------------------------------------------------------------

/// A function or object of a provider config for JavaScript: the extension's own value when it
/// came from this runtime, otherwise a `{ "$native": id }` proxy over the Rust object.
fn config_part<T: ?Sized>(bridge: &Bridge, object: &Arc<T>, native: impl FnOnce() -> Native) -> Value {
    match bridge.origins.get(object) {
        Some(origin) => origin,
        None => json!({ "$native": bridge.natives.insert(native()) }),
    }
}

/// A registered provider config as the JSON `providerConfigFromHost` (pi-ai/rust.js) turns into
/// the config object TS's `getRegisteredProviderConfig()` returns: only defined fields, the
/// extension's own functions and objects.
pub(crate) fn provider_config_to_js(bridge: &Arc<Bridge>, config: &ProviderConfig) -> Value {
    let mut out = Map::new();
    if let Some(v) = &config.name {
        out.insert("name".into(), json!(v));
    }
    if let Some(v) = &config.base_url {
        out.insert("baseUrl".into(), json!(v));
    }
    if let Some(v) = &config.api_key {
        out.insert("apiKey".into(), json!(v));
    }
    if let Some(v) = &config.api {
        out.insert("api".into(), json!(v));
    }
    if let Some(f) = &config.stream_simple {
        out.insert("streamSimple".into(), config_part(bridge, f, || Native::StreamSimple(f.clone())));
    }
    if let Some(map) = &config.images {
        let parts: Map<String, Value> = map
            .iter()
            .map(|(api, images)| (api.clone(), config_part(bridge, images, || Native::Images(images.clone()))))
            .collect();
        out.insert("images".into(), Value::Object(parts));
    }
    if let Some(map) = &config.classifiers {
        let parts: Map<String, Value> = map
            .iter()
            .map(|(api, classifier)| (api.clone(), config_part(bridge, classifier, || Native::Classifier(classifier.clone()))))
            .collect();
        out.insert("classifiers".into(), Value::Object(parts));
    }
    if let Some(v) = &config.headers {
        out.insert("headers".into(), serde_json::to_value(v).unwrap_or(Value::Null));
    }
    if let Some(v) = config.auth_header {
        out.insert("authHeader".into(), json!(v));
    }
    if let Some(v) = &config.models {
        out.insert("models".into(), serde_json::to_value(v).unwrap_or(Value::Null));
    }
    if let Some(f) = &config.refresh_models {
        out.insert("refreshModels".into(), config_part(bridge, f, || Native::Refresh(f.clone())));
    }
    if let Some(oauth) = &config.oauth {
        let value = match bridge.origins.get(&oauth.login) {
            Some(origin) => origin,
            None => {
                let mut o = json!({
                    "name": oauth.name,
                    "$oauth": bridge.natives.insert(Native::OAuth(oauth.clone())),
                    "modifyModels": oauth.modify_models.is_some(),
                });
                if let Some(v) = oauth.is_subscription {
                    o["isSubscription"] = json!(v);
                }
                if let Some(v) = oauth.uses_callback_server {
                    o["usesCallbackServer"] = json!(v);
                }
                o
            }
        };
        out.insert("oauth".into(), value);
    }
    Value::Object(out)
}

/// JavaScript legacy OAuth login callbacks (`{ onAuth, onPrompt, ... }`) for a Rust login.
struct JsOAuthLoginCallbacks {
    handle: JsHandle,
    manual: bool,
    signal: Option<CancellationToken>,
}

#[async_trait]
impl pi_ai::auth::extension_oauth_types::OAuthLoginCallbacks for JsOAuthLoginCallbacks {
    fn on_auth(&self, info: pi_ai::auth::extension_oauth_types::OAuthAuthInfo) {
        super::api::post_call(&self.handle, Some("onAuth"), vec![serde_json::to_value(info).unwrap_or(Value::Null)]);
    }

    fn on_device_code(&self, info: pi_ai::auth::extension_oauth_types::OAuthDeviceCodeInfo) {
        super::api::post_call(&self.handle, Some("onDeviceCode"), vec![serde_json::to_value(info).unwrap_or(Value::Null)]);
    }

    async fn on_prompt(&self, prompt: pi_ai::auth::extension_oauth_types::OAuthPrompt) -> anyhow::Result<String> {
        let value = self.handle.call_method("onPrompt", vec![serde_json::to_value(prompt)?]).await.map_err(super::js_error)?;
        Ok(value.as_str().unwrap_or_default().to_string())
    }

    fn on_progress(&self, message: &str) {
        super::api::post_call(&self.handle, Some("onProgress"), vec![json!(message)]);
    }

    async fn on_manual_code_input(&self) -> Option<anyhow::Result<String>> {
        if !self.manual {
            return None;
        }
        Some(
            self.handle
                .call_method("onManualCodeInput", vec![])
                .await
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .map_err(super::js_error),
        )
    }

    async fn on_select(&self, prompt: pi_ai::auth::extension_oauth_types::OAuthSelectPrompt) -> anyhow::Result<Option<String>> {
        let value = self.handle.call_method("onSelect", vec![serde_json::to_value(prompt)?]).await.map_err(super::js_error)?;
        Ok(value.as_str().map(str::to_string))
    }

    fn signal(&self) -> Option<CancellationToken> {
        self.signal.clone()
    }
}

fn resolve_input(bridge: &Arc<Bridge>, payload: &Value) -> ApiKeyResolveInput {
    let ctx: Arc<dyn AuthContext> = match bridge.handle(&payload["ctx"]) {
        Some(handle) => Arc::new(JsAuthContext { bridge: bridge.clone(), handle }),
        None => pi_ai::auth::context::default_provider_auth_context(),
    };
    ApiKeyResolveInput {
        ctx,
        credential: payload["credential"].is_object().then(|| api_key_credential_from_js(&payload["credential"])),
        signal: bridge.token(&payload["signal"]).unwrap_or_default(),
    }
}

pub(crate) fn js_interaction(bridge: &Arc<Bridge>, payload: &Value) -> anyhow::Result<ProviderAuthInteraction> {
    let handle = bridge.handle(&payload["interaction"]).ok_or_else(|| anyhow::anyhow!("login() expects an interaction"))?;
    Ok(ProviderAuthInteraction {
        interaction: Arc::new(JsAuthInteraction { bridge: bridge.clone(), handle }),
        signal: bridge.token(&payload["signal"]).unwrap_or_default(),
    })
}

pub(crate) fn dispatch(bridge: &Arc<Bridge>, op: &str, payload: Value) -> anyhow::Result<Value> {
    match op {
        "aip.p.getModels" => Ok(serde_json::to_value(provider_of(bridge, &payload)?.get_models())?),
        "aip.p.getAllModels" => Ok(serde_json::to_value(provider_of(bridge, &payload)?.get_all_models())?),
        "aip.p.filterModels" => {
            let provider = provider_of(bridge, &payload)?;
            let models: Vec<Model> = serde_json::from_value(payload["models"].clone())?;
            let credential: Option<Credential> = opt(&payload["credential"])?;
            Ok(serde_json::to_value(provider.filter_models(&models, credential.as_ref()).unwrap_or(models))?)
        }
        "aip.p.filterAllModels" => {
            let provider = provider_of(bridge, &payload)?;
            let models: Vec<AnyModel> = serde_json::from_value(payload["models"].clone())?;
            let credential: Option<Credential> = opt(&payload["credential"])?;
            Ok(serde_json::to_value(provider.filter_all_models(&models, credential.as_ref()).unwrap_or(models))?)
        }
        "aip.p.stream" => {
            let model: Model = serde_json::from_value(payload["model"].clone())?;
            let options = options_from_js(bridge, &payload["options"]);
            let method = str_arg(&payload, "method");
            let stream = match bridge.natives.get(payload["id"].as_u64().unwrap_or_default()) {
                Some(Native::Provider(provider)) => match method.as_str() {
                    "fetchDeferred" => provider.fetch_deferred(&model, &serde_json::from_value(payload["handle"].clone())?, options),
                    "streamSimple" => provider.stream_simple(&model, &transcript_from_js(&payload["context"])?, options),
                    _ => provider.stream(&model, &transcript_from_js(&payload["context"])?, options),
                },
                Some(Native::Streams(streams)) => match method.as_str() {
                    "fetchDeferred" => streams.fetch_deferred(&model, &serde_json::from_value(payload["handle"].clone())?, options),
                    "streamSimple" => streams.stream_simple(&model, &transcript_from_js(&payload["context"])?, options),
                    _ => streams.stream(&model, &transcript_from_js(&payload["context"])?, options),
                },
                Some(Native::StreamSimple(stream_simple)) => {
                    stream_simple(model, transcript_from_js(&payload["context"])?, Some(options))
                }
                _ => anyhow::bail!("unknown provider"),
            };
            Ok(json!(register_stream(bridge, stream)))
        }
        "aip.api" => {
            // Built-in API implementations (`anthropicMessagesApi()`, ...), by api id.
            let api = str_arg(&payload, "api");
            let streams = pi_ai::api::registry::builtin_api(&api);
            Ok(json!({
                "native": bridge.natives.insert(Native::Streams(streams.clone())),
                "fetchDeferred": streams.supports_fetch_deferred(),
                "cancelDeferred": streams.supports_cancel_deferred(),
            }))
        }
        "aip.builtinProviders" => Ok(Value::Array(
            pi_ai::providers::all::builtin_providers().into_iter().map(|p| describe_provider(bridge, p)).collect(),
        )),
        "aip.radiusProvider" => {
            let o = &payload["options"];
            let options = pi_ai::providers::radius::RadiusProviderOptions {
                id: o["id"].as_str().map(str::to_string),
                name: o["name"].as_str().map(str::to_string),
                gateway: o["gateway"].as_str().map(str::to_string),
            };
            Ok(describe_provider(bridge, pi_ai::providers::radius::radius_provider(options)))
        }
        "aip.cfg.oauthSync" => {
            let Some(Native::OAuth(oauth)) = bridge.natives.get(payload["id"].as_u64().unwrap_or_default()) else {
                anyhow::bail!("unknown oauth config");
            };
            let credentials: pi_ai::auth::types::OAuthCredentials = serde_json::from_value(payload["credentials"].clone())?;
            match str_arg(&payload, "m").as_str() {
                "getApiKey" => Ok(json!((oauth.get_api_key)(&credentials))),
                "modifyModels" => {
                    let models: Vec<Model> = serde_json::from_value(payload["models"].clone())?;
                    let modify = oauth.modify_models.as_ref().ok_or_else(|| anyhow::anyhow!("modifyModels is not defined"))?;
                    Ok(serde_json::to_value(modify(models, &credentials))?)
                }
                m => Err(anyhow::anyhow!("unknown oauth method {m}")),
            }
        }
        "aip.interaction.notify" => {
            if let Some(Native::Interaction(interaction)) = bridge.natives.get(payload["id"].as_u64().unwrap_or_default()) {
                interaction.interaction.notify(auth_event_from_js(&payload["event"]));
            }
            Ok(Value::Null)
        }
        "aip.release" => {
            if let Some(id) = payload["id"].as_u64() {
                bridge.natives.remove(id);
            }
            Ok(Value::Null)
        }
        "aip.compat.register" => {
            let api = str_arg(&payload, "api");
            let streams = native_streams(bridge, &payload["streams"]).ok_or_else(|| anyhow::anyhow!("registerApiProvider() expects stream functions"))?;
            pi_ai::compat::register_api_provider(&api, streams, payload["sourceId"].as_str());
            Ok(Value::Null)
        }
        "aip.compat.get" => Ok(match pi_ai::compat::get_api_provider(&str_arg(&payload, "api")) {
            Some(entry) => json!({ "api": entry.api, "native": bridge.natives.insert(Native::Streams(entry.streams)) }),
            None => Value::Null,
        }),
        "aip.compat.list" => Ok(Value::Array(
            pi_ai::compat::get_api_providers()
                .into_iter()
                .map(|entry| json!({ "api": entry.api, "native": bridge.natives.insert(Native::Streams(entry.streams)) }))
                .collect(),
        )),
        "aip.compat.unregister" => {
            pi_ai::compat::unregister_api_providers(&str_arg(&payload, "sourceId"));
            Ok(Value::Null)
        }
        "aip.compat.reset" => {
            pi_ai::compat::reset_api_providers();
            Ok(Value::Null)
        }
        "aip.compat.registerBuiltins" => {
            pi_ai::compat::register_builtin_api_providers();
            Ok(Value::Null)
        }
        "aip.compat.stream" => {
            let model: Model = serde_json::from_value(payload["model"].clone())?;
            let context: pi_ai::types::Context = serde_json::from_value(payload["context"].clone())?;
            let options = options_from_js(bridge, &payload["options"]);
            let stream = if payload["simple"].as_bool().unwrap_or(false) {
                pi_ai::compat::stream_simple(&model, &context, options)
            } else {
                pi_ai::compat::stream(&model, &context, options)
            };
            Ok(json!(register_stream(bridge, stream)))
        }
        "aip.images.register" => {
            let api = str_arg(&payload, "api");
            let handle = bridge.handle(&payload["object"]).ok_or_else(|| anyhow::anyhow!("registerImagesApiProvider() expects generateImages"))?;
            pi_ai::images_api_registry::register_images_api_provider(
                pi_ai::images_api_registry::ImagesApiProvider { api, implementation: Arc::new(JsImages::new(bridge, handle)) },
                payload["sourceId"].as_str(),
            );
            Ok(Value::Null)
        }
        "aip.images.registerBuiltins" => {
            pi_ai::images_api_registry::register_builtin_images_api_providers();
            Ok(Value::Null)
        }
        "aip.images.has" => Ok(json!(pi_ai::images_api_registry::get_images_api_provider(&str_arg(&payload, "api")).is_some())),
        _ => Err(anyhow::anyhow!("unknown host call {op}")),
    }
}

pub(crate) async fn dispatch_async(bridge: Arc<Bridge>, op: &str, payload: Value) -> anyhow::Result<Value> {
    match op {
        "aip.ctx" => {
            let Some(Native::AuthContext(ctx)) = bridge.natives.get(payload["id"].as_u64().unwrap_or_default()) else {
                anyhow::bail!("This auth context is no longer available");
            };
            let arg = payload["a"][0].as_str().unwrap_or_default().to_string();
            Ok(match str_arg(&payload, "m").as_str() {
                "env" => json!(ctx.env(&arg).await),
                "fileExists" => json!(ctx.file_exists(&arg).await),
                m => anyhow::bail!("authContext.{m} is not available"),
            })
        }
        "aip.interaction.prompt" => {
            let Some(Native::Interaction(interaction)) = bridge.natives.get(payload["id"].as_u64().unwrap_or_default()) else {
                anyhow::bail!("This login interaction is no longer available");
            };
            let prompt = auth_prompt_from_js(&bridge, &payload["prompt"]);
            Ok(json!(interaction.interaction.prompt(prompt).await?))
        }
        "aip.p.auth" => {
            let provider = provider_of(&bridge, &payload)?;
            let auth = provider.auth().clone();
            match str_arg(&payload, "m").as_str() {
                "apiKey.resolve" => {
                    let api_key = auth.api_key.ok_or_else(|| anyhow::anyhow!("provider has no api-key auth"))?;
                    Ok(api_key.resolve(resolve_input(&bridge, &payload)).await?.map(|r| auth_result_to_js(&r)).unwrap_or(Value::Null))
                }
                "apiKey.check" => {
                    let api_key = auth.api_key.ok_or_else(|| anyhow::anyhow!("provider has no api-key auth"))?;
                    Ok(api_key.check(resolve_input(&bridge, &payload)).await?.map(|c| auth_check_to_js(&c)).unwrap_or(Value::Null))
                }
                "apiKey.login" => {
                    let api_key = auth.api_key.ok_or_else(|| anyhow::anyhow!("provider has no api-key auth"))?;
                    Ok(api_key_credential_to_js(&api_key.login(js_interaction(&bridge, &payload)?).await?))
                }
                "oauth.login" => {
                    let oauth = auth.oauth.ok_or_else(|| anyhow::anyhow!("provider has no OAuth auth"))?;
                    let options = &payload["options"];
                    let login_options = options.is_object().then(|| LoginOptions {
                        get_device_id: bridge.handle(&options["getDeviceId"]).map(|h| {
                            Arc::new(move || h.call_blocking(vec![]).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default())
                                as Arc<dyn Fn() -> String + Send + Sync>
                        }),
                        agent_name: options["agentName"].as_str().map(str::to_string),
                    });
                    Ok(oauth_credential_to_js(&oauth.login(js_interaction(&bridge, &payload)?, login_options).await?))
                }
                "oauth.refresh" => {
                    let oauth = auth.oauth.ok_or_else(|| anyhow::anyhow!("provider has no OAuth auth"))?;
                    let credential = oauth_credential_from_js(&payload["credential"])?;
                    let signal = bridge.token(&payload["signal"]).unwrap_or_default();
                    Ok(oauth_credential_to_js(&oauth.refresh(credential, signal).await?))
                }
                "oauth.toAuth" => {
                    let oauth = auth.oauth.ok_or_else(|| anyhow::anyhow!("provider has no OAuth auth"))?;
                    Ok(model_auth_to_js(&oauth.to_auth(&oauth_credential_from_js(&payload["credential"])?).await?))
                }
                m => anyhow::bail!("auth.{m} is not available"),
            }
        }
        "aip.p.refreshModels" => {
            let native = bridge.natives.get(payload["id"].as_u64().unwrap_or_default());
            let ctx = &payload["context"];
            let publish = bridge.handle(&ctx["publish"]).ok_or_else(|| anyhow::anyhow!("refreshModels() needs context.publish"))?;
            let b = bridge.clone();
            let publish_fn: pi_ai::models::PublishFn = Arc::new(move |publication: ModelsPublication| {
                let b = b.clone();
                let publish = publish.clone();
                Box::pin(async move {
                    let mut value = Map::new();
                    match publication.persist {
                        None => {}
                        Some(None) => {
                            value.insert("persist".into(), Value::Null);
                        }
                        Some(Some(entry)) => {
                            value.insert("persist".into(), serde_json::to_value(entry)?);
                        }
                    }
                    let update = publication.update.map(|f| {
                        let f = parking_lot::Mutex::new(Some(f));
                        b.register_callback(Arc::new(move |_| {
                            if let Some(f) = f.lock().take() {
                                f();
                            }
                            Ok(Value::Null)
                        }))
                    });
                    if let Some(id) = update {
                        value.insert("$update".into(), json!(id));
                    }
                    let result = b.glue_call("invokePublish", vec![publish.to_value(), Value::Object(value)]).await;
                    if let Some(id) = update {
                        b.callbacks.remove(id);
                    }
                    Ok(result?.as_bool().unwrap_or(false))
                }) as BoxFuture<'static, anyhow::Result<bool>>
            });
            let context = RefreshModelsContext {
                credential: opt(&ctx["credential"])?,
                stored: opt(&ctx["stored"])?,
                publish: publish_fn,
                allow_network: ctx["allowNetwork"].as_bool().unwrap_or(false),
                force: ctx["force"].as_bool(),
                signal: bridge.token(&ctx["signal"]).unwrap_or_default(),
                base_url: ctx["baseUrl"].as_str().map(str::to_string),
            };
            match native {
                Some(Native::Provider(provider)) => {
                    provider.refresh_models(context).await?;
                    Ok(Value::Null)
                }
                Some(Native::Refresh(refresh)) => Ok(serde_json::to_value(refresh(context).await?)?),
                _ => Err(anyhow::anyhow!("unknown provider")),
            }
        }
        "aip.p.cancelDeferred" => {
            let model: Model = serde_json::from_value(payload["model"].clone())?;
            let handle: DeferredHandle = serde_json::from_value(payload["handle"].clone())?;
            let options = options_from_js(&bridge, &payload["options"]);
            match bridge.natives.get(payload["id"].as_u64().unwrap_or_default()) {
                Some(Native::Provider(p)) => p.cancel_deferred(&model, &handle, options).await?,
                Some(Native::Streams(s)) => s.cancel_deferred(&model, &handle, options).await?,
                _ => anyhow::bail!("unknown provider"),
            }
            Ok(Value::Null)
        }
        "aip.p.generateImages" => {
            let native = bridge.natives.get(payload["id"].as_u64().unwrap_or_default());
            let model: ImageModel = serde_json::from_value(payload["model"].clone())?;
            let context: ImagesContext = serde_json::from_value(payload["context"].clone())?;
            let options = options_from_js(&bridge, &payload["options"]);
            let result = match native {
                Some(Native::Provider(provider)) => provider.generate_images(&model, &context, options).await,
                Some(Native::Images(images)) => images.generate_images(&model, &context, options).await,
                _ => anyhow::bail!("unknown provider"),
            };
            Ok(serde_json::to_value(result)?)
        }
        "aip.p.classify" => {
            let native = bridge.natives.get(payload["id"].as_u64().unwrap_or_default());
            let model: ClassifierModel = serde_json::from_value(payload["model"].clone())?;
            let context: ClassifierContext = serde_json::from_value(payload["context"].clone())?;
            let options = options_from_js(&bridge, &payload["options"]);
            let result = match native {
                Some(Native::Provider(provider)) => provider.classify(&model, &context, options).await,
                Some(Native::Classifier(classifier)) => classifier.classify(&model, &context, options).await,
                _ => anyhow::bail!("unknown provider"),
            };
            Ok(serde_json::to_value(result)?)
        }
        "aip.cfg.oauth" => {
            let Some(Native::OAuth(oauth)) = bridge.natives.get(payload["id"].as_u64().unwrap_or_default()) else {
                anyhow::bail!("unknown oauth config");
            };
            let credentials = || serde_json::from_value::<pi_ai::auth::types::OAuthCredentials>(payload["credentials"].clone());
            match str_arg(&payload, "m").as_str() {
                "login" => {
                    let handle = bridge.handle(&payload["callbacks"]).ok_or_else(|| anyhow::anyhow!("login() expects callbacks"))?;
                    let callbacks = Arc::new(JsOAuthLoginCallbacks {
                        handle,
                        manual: payload["manual"].as_bool().unwrap_or(false),
                        signal: bridge.token(&payload["signal"]),
                    });
                    Ok(serde_json::to_value((oauth.login)(callbacks).await?)?)
                }
                "refreshToken" => {
                    let signal = bridge.token(&payload["signal"]).unwrap_or_default();
                    Ok(serde_json::to_value((oauth.refresh_token)(credentials()?, signal).await?)?)
                }
                m => Err(anyhow::anyhow!("unknown oauth method {m}")),
            }
        }
        "aip.p.stream" | "aip.compat.stream" => dispatch(&bridge, op, payload),
        "aip.images.openrouter" => {
            let model: ImageModel = serde_json::from_value(payload["model"].clone())?;
            let context: ImagesContext = serde_json::from_value(payload["context"].clone())?;
            let options = options_from_js(&bridge, &payload["options"]);
            let api = pi_ai::api::registry::builtin_images_api("openrouter-images");
            Ok(serde_json::to_value(api.generate_images(&model, &context, options).await)?)
        }
        "aip.images.generate" => {
            let model: ImageModel = serde_json::from_value(payload["model"].clone())?;
            let context: ImagesContext = serde_json::from_value(payload["context"].clone())?;
            let options = options_from_js(&bridge, &payload["options"]);
            Ok(serde_json::to_value(pi_ai::images::generate_images(&model, &context, options).await?)?)
        }
        _ => Err(anyhow::anyhow!("unknown async host call {op}")),
    }
}
