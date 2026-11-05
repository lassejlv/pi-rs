//! OpenAI Responses API (port of `api/openai-responses.ts`).
//!
//! Implemented directly against the HTTP API (no SDK). Requests mirror what the `openai` JS SDK
//! sends for `client.responses.create({ ..., stream: true })`: `POST {baseUrl}/responses` with
//! `Authorization: Bearer`, the Stainless headers and a JSON body.
//!
//! API-specific options are read from `StreamOptions.extra`: `reasoningEffort`,
//! `reasoningSummary`, `serviceTier`, `toolChoice`.

use std::collections::HashMap;

use futures::Stream;
use serde_json::{Map, Value, json};

use super::constrained_sampling::create_grammar_tool_input_properties;
use super::github_copilot_headers::{build_copilot_dynamic_headers, has_copilot_vision_input};
use super::openai_prompt_cache::clamp_openai_prompt_cache_key;
use super::openai_responses_shared::{
    ConvertResponsesMessagesOptions, ConvertResponsesToolsOptions, OpenAIResponsesStreamOptions,
    apply_service_tier_multiplier, convert_responses_messages, convert_responses_tools,
    get_responses_tool_support, process_responses_stream,
};
use super::simple_options::{build_base_options, resolve_sampling_params};
use crate::models::{ProviderStreams, clamp_thinking_level};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, CacheRetention, Model, ModelThinkingLevel,
    ProviderEnv, ProviderHeaders, ProviderResponse, SessionAffinityFormat, StopReason,
    StreamOptions, TranscriptContext, Usage,
};
use crate::utils::error_body::{
    ProviderHttpError, SdkErrorStyle, format_provider_error, normalize_provider_error,
};
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};
use crate::utils::headers::headers_to_record;
use crate::utils::http::{
    HeaderList, apply_openai_custom_headers_env, apply_openai_sdk_tenant_headers, client_for, post_json,
    stainless_headers,
};
use crate::utils::identity::{ClientIdentityTarget, get_identity_headers};
use crate::utils::provider_env::get_provider_env_value;
use crate::utils::provider_retry::{ProviderRetryOptions, retry_provider_request};
use crate::utils::sse::sse_reader;
use crate::utils::transcript::{get_declared_tools, resolve_transcript, resolve_transcript_tools};

/// Version of the `openai` JS SDK whose request shape this module reproduces.
pub const OPENAI_SDK_VERSION: &str = "7.19.0";
const DEFAULT_TIMEOUT_MS: u64 = 600_000;

const OPENAI_TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];
/// OpenAI Responses rejects max_output_tokens below 16.
pub const OPENAI_RESPONSES_MIN_OUTPUT_TOKENS: u64 = 16;
const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/settings/usage";
const CHATGPT_WORKSPACE_LIMIT_HINT: &str = "You've reached your ChatGPT workspace's usage limit. Ask your workspace admin to increase it at admin.openai.com → Workspace → Usage limits.";
/// Fixed reasoning.effort while configuration_update items carry the thinking level.
const CONFIGURATION_UPDATE_REQUEST_EFFORT: &str = "medium";

/// OpenAI API keys start with `sk-`; a different credential sent directly to OpenAI is a
/// Sign in with ChatGPT access token.
fn is_chatgpt_sign_in(model: &Model, api_key: Option<&str>) -> bool {
    model.provider == "openai"
        && model.base_url == "https://api.openai.com/v1"
        && api_key.is_some_and(|k| !k.starts_with("sk-"))
}

fn has_header(headers: Option<&ProviderHeaders>, name: &str) -> bool {
    headers.is_some_and(|h| {
        h.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case(name) && v.as_ref().is_some_and(|v| !v.trim().is_empty())
        })
    })
}

fn get_client_api_key(
    provider: &str,
    api_key: Option<&str>,
    headers: Option<&ProviderHeaders>,
) -> anyhow::Result<String> {
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        return Ok(key.to_string());
    }
    if has_header(headers, "authorization") || has_header(headers, "cf-aig-authorization") {
        return Ok("unused".to_string());
    }
    anyhow::bail!("No API key for provider: {provider}")
}

/// Resolve cache retention: defaults to short; `PI_CACHE_RETENTION=long` opts in.
pub(crate) fn resolve_cache_retention(
    cache_retention: Option<CacheRetention>,
    env: Option<&ProviderEnv>,
) -> CacheRetention {
    if let Some(retention) = cache_retention {
        return retention;
    }
    if get_provider_env_value("PI_CACHE_RETENTION", env).as_deref() == Some("long") {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

/// Resolved `OpenAIResponsesCompat`.
#[derive(Debug, Clone)]
struct ResponsesCompat {
    supports_mid_convo_system_messages: bool,
    session_affinity_format: SessionAffinityFormat,
    supports_long_cache_retention: bool,
    supports_strict_mode: bool,
    supports_openai_grammar_tools: bool,
    supports_additional_tools: bool,
    supports_tool_search: bool,
    supports_explicit_prompt_cache_mode: bool,
    supports_max_output_tokens: bool,
    supports_mid_convo_effort: bool,
}

fn get_compat(model: &Model) -> ResponsesCompat {
    let c = model.compat();
    let detected = if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
        SessionAffinityFormat::Openrouter
    } else {
        SessionAffinityFormat::Openai
    };
    ResponsesCompat {
        supports_mid_convo_system_messages: c.supports_mid_convo_system_messages.unwrap_or(false),
        session_affinity_format: c.session_affinity_format.unwrap_or(detected),
        supports_long_cache_retention: c.supports_long_cache_retention.unwrap_or(true),
        supports_strict_mode: c.supports_strict_mode.unwrap_or(false),
        supports_openai_grammar_tools: c.supports_openai_grammar_tools.unwrap_or(false),
        supports_additional_tools: c.supports_additional_tools.unwrap_or(false),
        supports_tool_search: c.supports_tool_search.unwrap_or(false),
        supports_explicit_prompt_cache_mode: c.supports_explicit_prompt_cache_mode.unwrap_or(false),
        supports_max_output_tokens: c.supports_max_output_tokens.unwrap_or(true),
        supports_mid_convo_effort: c.supports_mid_convo_effort.unwrap_or(false),
    }
}

/// `model.thinkingLevelMap?.[level]`: `None` when absent, `Some(None)` when mapped to null.
pub(crate) fn thinking_level_entry(model: &Model, level: &str) -> Option<Option<String>> {
    let level: ModelThinkingLevel = serde_json::from_value(json!(level)).ok()?;
    model
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get(&level))
        .cloned()
}

/// `model.thinkingLevelMap?.[level] ?? level`.
pub(crate) fn mapped_thinking_level(model: &Model, level: &str) -> String {
    thinking_level_entry(model, level)
        .flatten()
        .unwrap_or_else(|| level.to_string())
}

/// Typed view of the Responses-specific options carried in `StreamOptions.extra`.
#[derive(Debug, Clone, Default)]
struct ResponsesOptions {
    reasoning_effort: Option<String>,
    reasoning_summary: Option<String>,
    service_tier: Option<String>,
    tool_choice: Option<Value>,
}

impl ResponsesOptions {
    fn from(options: &StreamOptions) -> Self {
        Self {
            reasoning_effort: options
                .extra_str("reasoningEffort")
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            reasoning_summary: options
                .extra_str("reasoningSummary")
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            service_tier: options
                .extra_value("serviceTier")
                .and_then(Value::as_str)
                .map(str::to_string),
            // `toolChoice !== undefined`: an explicit null is sent.
            tool_choice: options.extra_value("toolChoice").cloned(),
        }
    }
}

fn get_configuration_update_effort(
    model: &Model,
    options: &StreamOptions,
    responses: &ResponsesOptions,
    compat: &ResponsesCompat,
) -> Option<String> {
    let effort = responses.reasoning_effort.as_deref()?;
    if !compat.supports_mid_convo_effort || is_chatgpt_sign_in(model, options.api_key.as_deref()) {
        return None;
    }
    Some(mapped_thinking_level(model, effort))
}

fn get_prompt_cache_retention(
    compat: &ResponsesCompat,
    retention: CacheRetention,
) -> Option<&'static str> {
    (retention == CacheRetention::Long
        && compat.supports_long_cache_retention
        && !compat.supports_explicit_prompt_cache_mode)
        .then_some("24h")
}

fn get_prompt_cache_options(compat: &ResponsesCompat, retention: CacheRetention) -> Option<Value> {
    if !compat.supports_explicit_prompt_cache_mode {
        return None;
    }
    match retention {
        CacheRetention::None => Some(json!({ "mode": "explicit" })),
        CacheRetention::Long if compat.supports_long_cache_retention => {
            Some(json!({ "ttl": "30m" }))
        }
        _ => None,
    }
}

fn build_params(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    responses: &ResponsesOptions,
    compat: &ResponsesCompat,
    grammar_tool_input_properties: &HashMap<String, String>,
    configuration_update_effort: Option<&str>,
) -> anyhow::Result<Value> {
    let tool_support = get_responses_tool_support(
        compat.supports_mid_convo_system_messages,
        compat.supports_additional_tools,
        compat.supports_tool_search,
    );
    let transcript_tools = resolve_transcript_tools(&context.messages, tool_support);
    let tool_options = ConvertResponsesToolsOptions {
        supports_strict_mode: Some(compat.supports_strict_mode),
        supports_openai_grammar_tools: compat.supports_openai_grammar_tools,
        ..Default::default()
    };
    let messages = convert_responses_messages(
        model,
        context,
        &OPENAI_TOOL_CALL_PROVIDERS,
        &ConvertResponsesMessagesOptions {
            grammar_tool_input_properties: Some(grammar_tool_input_properties),
            supports_mid_convo_system_messages: compat.supports_mid_convo_system_messages,
            supports_additional_tools: compat.supports_additional_tools,
            supports_tool_search: compat.supports_tool_search,
            tool_options: tool_options.clone(),
            configuration_update_effort: configuration_update_effort.map(str::to_string),
            ..Default::default()
        },
    )?;

    let retention = resolve_cache_retention(options.cache_retention, options.env.as_ref());
    let omit_unsupported_fields = is_chatgpt_sign_in(model, options.api_key.as_deref());
    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("input".into(), Value::Array(messages));
    params.insert("stream".into(), json!(true));
    if retention != CacheRetention::None
        && let Some(key) = clamp_openai_prompt_cache_key(options.session_id.as_deref())
    {
        params.insert("prompt_cache_key".into(), json!(key));
    }
    if !omit_unsupported_fields {
        if let Some(value) = get_prompt_cache_retention(compat, retention) {
            params.insert("prompt_cache_retention".into(), json!(value));
        }
        if let Some(value) = get_prompt_cache_options(compat, retention) {
            params.insert("prompt_cache_options".into(), value);
        }
    }
    params.insert("store".into(), json!(false));

    if let Some(max_tokens) = options.max_tokens.filter(|m| *m > 0)
        && compat.supports_max_output_tokens
        && !omit_unsupported_fields
    {
        params.insert(
            "max_output_tokens".into(),
            json!(max_tokens.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS)),
        );
    }
    if let Some(temperature) = options.temperature
        && !omit_unsupported_fields
    {
        params.insert(
            "temperature".into(),
            crate::serde_util::js_number(temperature),
        );
    }
    // `serviceTier !== undefined`: an explicit null is sent.
    if let Some(tier) = options.extra_value("serviceTier") {
        params.insert("service_tier".into(), tier.clone());
    }
    if !transcript_tools.request_tools.is_empty() {
        params.insert(
            "tools".into(),
            Value::Array(convert_responses_tools(
                &transcript_tools.request_tools,
                &tool_options,
            )?),
        );
    }
    if let Some(choice) = &responses.tool_choice {
        params.insert("tool_choice".into(), choice.clone());
    }

    let reasoning_effort = responses.reasoning_effort.clone().or_else(|| {
        responses
            .reasoning_summary
            .as_ref()
            .map(|_| "medium".to_string())
    });
    if model.reasoning {
        if let Some(effort) = &reasoning_effort {
            let requested = match &responses.reasoning_effort {
                Some(requested) => mapped_thinking_level(model, requested),
                None => effort.clone(),
            };
            let effort = if configuration_update_effort.is_none() {
                requested
            } else {
                CONFIGURATION_UPDATE_REQUEST_EFFORT.to_string()
            };
            params.insert(
                "reasoning".into(),
                json!({ "effort": effort, "summary": responses.reasoning_summary.clone().unwrap_or_else(|| "auto".into()) }),
            );
            params.insert("include".into(), json!(["reasoning.encrypted_content"]));
        } else if model.provider != "github-copilot" {
            match thinking_level_entry(model, "off") {
                Some(None) => {}
                Some(Some(off)) => {
                    params.insert("reasoning".into(), json!({ "effort": off }));
                }
                None => {
                    params.insert("reasoning".into(), json!({ "effort": "none" }));
                }
            }
        }
        if model.provider == "xai" {
            params.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
    }

    let level = reasoning_effort
        .as_deref()
        .and_then(|e| serde_json::from_value::<ModelThinkingLevel>(json!(e)).ok())
        .unwrap_or(ModelThinkingLevel::Off);
    if let Some(sampling) = resolve_sampling_params(model, level, options.sampling_params.as_ref())
    {
        for (k, v) in sampling {
            params.insert(k, v);
        }
    }
    Ok(Value::Object(params))
}

/// Request headers for the `openai` SDK client (`createClient`).
fn build_headers(
    model: &Model,
    context: &TranscriptContext,
    api_key: &str,
    options: &StreamOptions,
    session_id: Option<&str>,
    compat: &ResponsesCompat,
    timeout_ms: u64,
) -> anyhow::Result<HeaderList> {
    let mut headers = openai_sdk_base_headers(timeout_ms);
    headers.set("Authorization", Some(&format!("Bearer {api_key}")));
    apply_openai_custom_headers_env(&mut headers)?;
    let target = ClientIdentityTarget {
        provider: &model.provider,
        base_url: &model.base_url,
    };
    headers.extend_strings(&get_identity_headers(target, options.identity.as_ref()));
    if let Some(model_headers) = &model.headers {
        headers.extend_strings(model_headers);
    }
    if model.provider == "github-copilot" {
        let dynamic = build_copilot_dynamic_headers(
            &context.messages,
            has_copilot_vision_input(&context.messages),
        );
        headers.extend_strings(&dynamic);
    }
    if let Some(session_id) = session_id.filter(|s| !s.is_empty()) {
        if compat.session_affinity_format == SessionAffinityFormat::Openrouter {
            headers.set("x-session-id", Some(session_id));
        } else {
            if compat.session_affinity_format == SessionAffinityFormat::Openai {
                headers.set("session_id", Some(session_id));
            }
            headers.set("x-client-request-id", Some(session_id));
        }
    }
    if let Some(h) = &options.headers {
        headers.extend_optional(h);
    }
    headers.set("Content-Type", Some("application/json"));
    Ok(headers)
}

/// The `Accept`, `User-Agent` and Stainless headers the `openai` SDK sends on every request.
pub(crate) fn openai_sdk_base_headers(timeout_ms: u64) -> HeaderList {
    let mut headers = HeaderList::new();
    headers.set("Accept", Some("application/json"));
    headers.set(
        "User-Agent",
        Some(&format!("OpenAI/JS {OPENAI_SDK_VERSION}")),
    );
    for (name, value) in stainless_headers(OPENAI_SDK_VERSION, timeout_ms) {
        headers.set(&name, Some(&value));
    }
    apply_openai_sdk_tenant_headers(&mut headers);
    headers
}

/// `APIError.makeMessage` of the `openai` SDK for an error without a status: a truthy
/// `error.message` (`JSON.stringify`-ed unless a string), else the whole error as JSON, else
/// `message`.
fn sdk_error_message(error: Option<&Value>, message: Option<&Value>) -> String {
    use crate::utils::js_json::js_json_stringify;
    let msg = match error.filter(|e| truthy(e)) {
        Some(error) => match error.get("message").filter(|m| truthy(m)) {
            Some(Value::String(s)) => s.clone(),
            Some(m) => js_json_stringify(m),
            None => js_json_stringify(error),
        },
        // `message` is `data.message` as is: only a non-empty string prints (a template literal of
        // another truthy value would too, but the SSE `message` field is a string in practice).
        None => match message.filter(|m| truthy(m)) {
            Some(Value::String(s)) => s.clone(),
            Some(m) => js_json_stringify(m),
            None => String::new(),
        },
    };
    if msg.is_empty() { "(no status code or body)".to_string() } else { msg }
}

/// The error the `openai` SDK raises for an SSE frame whose data is not JSON.
pub(crate) const MALFORMED_SSE_JSON: &str = "Error reading response: malformed server-sent event JSON.";

/// Events of an `openai` SDK `Stream` over an SSE response (`Stream.fromSSEResponse`): `[DONE]`
/// ends it, an `error` event or a data object with a truthy `error` field raises an `APIError`,
/// `thread.*` events yield `{ event, data }`, and an abort of the request signal ends the stream
/// without an error (the SDK swallows transport aborts).
pub(crate) fn openai_sse_events(
    response: reqwest::Response,
    signal: Option<tokio_util::sync::CancellationToken>,
) -> impl Stream<Item = anyhow::Result<Value>> + Unpin + Send {
    let headers = response.headers().clone();
    let reader = sse_reader(response, signal);
    Box::pin(futures::stream::unfold(
        (reader, headers, false),
        |(mut reader, headers, done)| async move {
            if done {
                return None;
            }
            let api_error = |error: Option<Value>, message: Option<&Value>, headers: Option<&reqwest::header::HeaderMap>| {
                ProviderHttpError {
                    status: None,
                    headers: headers.cloned(),
                    message: sdk_error_message(error.as_ref(), message),
                    body: None,
                    error,
                    name: "APIError".to_string(),
                }
            };
            match reader.next_event().await {
                Err(error) if error.downcast_ref::<crate::utils::sse::StreamAborted>().is_some() => None,
                Err(error) => Some((Err(error), (reader, headers, true))),
                Ok(None) => None,
                Ok(Some(sse)) => {
                    if sse.data == "[DONE]" {
                        return None;
                    }
                    let data: Value = match serde_json::from_str(&sse.data) {
                        Ok(data) => data,
                        Err(_) => {
                            return Some((Err(anyhow::anyhow!(MALFORMED_SSE_JSON)), (reader, headers, true)));
                        }
                    };
                    if sse.event.as_deref().is_some_and(|e| e.starts_with("thread.")) {
                        let event = json!({ "event": sse.event, "data": data });
                        return Some((Ok(event), (reader, headers, false)));
                    }
                    if sse.event.as_deref() == Some("error") {
                        // `new APIError(undefined, data.error, data.message, undefined)`
                        let http = api_error(data.get("error").cloned(), data.get("message"), None);
                        return Some((Err(http.into()), (reader, headers, true)));
                    }
                    if let Some(error) = data.get("error").filter(|e| truthy(e)) {
                        // `new APIError(undefined, data.error, undefined, response.headers)`
                        let http = api_error(Some(error.clone()), None, Some(&headers));
                        return Some((Err(http.into()), (reader, headers, true)));
                    }
                    Some((Ok(data), (reader, headers, false)))
                }
            }
        },
    ))
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn get_service_tier_cost_multiplier(model: &Model, service_tier: Option<&str>) -> f64 {
    match service_tier {
        Some("flex") => 0.5,
        Some("priority") | Some("fast") => {
            if model.id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => 1.0,
    }
}

/// Error text as surfaced on the assistant message. An abort before the response surfaces as
/// `provider-retry`'s "Request aborted"; an abort mid-stream ends the SDK stream quietly and
/// fails the "Request was aborted" check after it.
pub(crate) fn error_message_of(error: &anyhow::Error, prefix: Option<&str>) -> String {
    format_provider_error(&normalize_provider_error(error), prefix)
}

async fn run_stream(
    model: Model,
    context: TranscriptContext,
    options: StreamOptions,
    sender: AssistantMessageEventSender,
) {
    let compat = get_compat(&model);
    let normalized = resolve_transcript(&context, Some(compat.supports_mid_convo_system_messages));
    let responses = ResponsesOptions::from(&options);
    let configuration_update_effort =
        get_configuration_update_effort(&model, &options, &responses, &compat);
    let mut output = AssistantMessage {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        provider_thinking_level: configuration_update_effort.clone(),
        stop_reason: StopReason::Pending,
        timestamp: crate::utils::now_ms(),
        ..Default::default()
    };
    let signal = options.signal.clone();
    let aborted = || signal.as_ref().is_some_and(|s| s.is_cancelled());

    let result: anyhow::Result<()> = async {
        let api_key = get_client_api_key(
            &model.provider,
            options.api_key.as_deref(),
            options.headers.as_ref(),
        )?;
        let retention = resolve_cache_retention(options.cache_retention, options.env.as_ref());
        let cache_session_id = if retention == CacheRetention::None {
            None
        } else {
            options.session_id.as_deref()
        };
        let grammar_props = create_grammar_tool_input_properties(
            &get_declared_tools(&normalized.messages),
            compat.supports_openai_grammar_tools,
        )?;
        let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let headers = build_headers(
            &model,
            &normalized,
            &api_key,
            &options,
            cache_session_id,
            &compat,
            timeout_ms,
        )?;
        let mut params = build_params(
            &model,
            &normalized,
            &options,
            &responses,
            &compat,
            &grammar_props,
            configuration_update_effort.as_deref(),
        )?;
        if let Some(on_payload) = &options.on_payload
            && let Some(next) = on_payload(params.clone(), model.clone()).await
        {
            params = next;
        }
        let url = format!("{}/responses", model.base_url.trim_end_matches('/'));
        let client = match &options.http_client {
            Some(client) => client.clone(),
            None => client_for(&url, options.env.as_ref())?,
        };
        let retry = ProviderRetryOptions {
            max_retries: options.max_retries,
            max_retry_delay_ms: options.max_retry_delay_ms,
            signal: signal.clone(),
            no_retry_statuses: Vec::new(),
        };
        let response = retry_provider_request(
            || {
                post_json(
                    &client,
                    &url,
                    &headers,
                    &params,
                    Some(timeout_ms),
                    signal.as_ref(),
                    SdkErrorStyle::OpenAI,
                )
            },
            &retry,
        )
        .await?;
        if let Some(on_response) = &options.on_response {
            on_response(
                ProviderResponse {
                    status: response.status().as_u16(),
                    headers: headers_to_record(response.headers()),
                },
                model.clone(),
            )
            .await;
        }
        sender.push(AssistantMessageEvent::Start {
            partial: output.clone(),
        });

        let pricing_model = model.clone();
        let apply = move |usage: &mut Usage, tier: Option<&str>| {
            apply_service_tier_multiplier(
                usage,
                get_service_tier_cost_multiplier(&pricing_model, tier),
            );
        };
        process_responses_stream(
            openai_sse_events(response, signal.clone()),
            &mut output,
            &sender,
            &model,
            OpenAIResponsesStreamOptions {
                on_provider_stream_event: options.on_provider_stream_event.clone(),
                service_tier: responses.service_tier.clone(),
                grammar_tool_input_properties: Some(&grammar_props),
                resolve_service_tier: None,
                apply_service_tier_pricing: Some(&apply),
            },
        )
        .await?;

        if aborted() {
            anyhow::bail!("Request was aborted");
        }
        match output.stop_reason {
            StopReason::Pending => {
                anyhow::bail!("OpenAI Responses stream ended without a stop reason")
            }
            StopReason::Aborted | StopReason::Error => {
                anyhow::bail!(
                    "{}",
                    output
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "An unknown error occurred".into())
                )
            }
            _ => {}
        }
        sender.push(AssistantMessageEvent::Done {
            reason: output.stop_reason,
            message: output.clone(),
        });
        sender.end(None);
        Ok(())
    }
    .await;

    if let Err(error) = result {
        output.stop_reason = if aborted() {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        let prefix = format!(
            "{} API error",
            if model.provider == "openai" {
                "OpenAI"
            } else {
                model.provider.as_str()
            }
        );
        let message = error_message_of(&error, Some(&prefix));
        output.error_message = Some(
            if message.contains("subscription_sharing_usage_limit_exceeded") {
                format!("{message}\nCheck your ChatGPT usage: {CHATGPT_USAGE_URL}")
            } else if message
                .contains("subscription_sharing_workplace_individual_usage_limit_exceeded")
            {
                format!("{message}\n{CHATGPT_WORKSPACE_LIMIT_HINT}")
            } else {
                message
            },
        );
        sender.push(AssistantMessageEvent::Error {
            reason: output.stop_reason,
            error: output.clone(),
        });
        sender.end(None);
    }
}

/// Stream an OpenAI Responses response.
pub fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    let (sender, stream) = AssistantMessageEventStream::new();
    crate::utils::wasm::spawn(run_stream(model.clone(), context.clone(), options, sender));
    stream
}

/// The clamped reasoning effort for `stream_simple` (`undefined` for off).
pub(crate) fn simple_reasoning_effort(model: &Model, options: &StreamOptions) -> Option<String> {
    let level = clamp_thinking_level(model, ModelThinkingLevel::from(options.reasoning?));
    (level != ModelThinkingLevel::Off).then(|| level.as_str().to_string())
}

/// Stream with provider-neutral reasoning options.
pub fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    if let Err(error) = get_client_api_key(
        &model.provider,
        options.api_key.as_deref(),
        options.headers.as_ref(),
    ) {
        return super::lazy::error_stream(model, error);
    }
    let mut base = build_base_options(model, context, &options, options.api_key.as_deref());
    if let Some(choice) = options.tool_choice {
        base.extra.insert(
            "toolChoice".into(),
            serde_json::to_value(choice).unwrap_or(Value::Null),
        );
    }
    if let Some(effort) = simple_reasoning_effort(model, &options) {
        base.extra.insert("reasoningEffort".into(), json!(effort));
    }
    stream(model, context, base)
}

/// The `openai-responses` API implementation.
pub struct OpenAIResponsesApi;

impl ProviderStreams for OpenAIResponsesApi {
    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        stream(model, context, options)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: StreamOptions,
    ) -> AssistantMessageEventStream {
        stream_simple(model, context, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_error_messages_match_api_error_make_message() {
        // Expected values from the `openai` SDK's `APIError.makeMessage(undefined, error, message)`.
        let parse = |text: &str| serde_json::from_str::<Value>(text).unwrap();
        let cases: [(Option<Value>, Option<Value>, &str); 7] = [
            (Some(parse(r#"{"message":{"b":1.0,"1":2}}"#)), None, r#"{"1":2,"b":1}"#),
            (None, Some(json!("boom")), "boom"),
            (Some(json!(false)), Some(json!("m")), "m"),
            (Some(parse(r#"{"message":0,"code":1.5}"#)), None, r#"{"message":0,"code":1.5}"#),
            (Some(json!("str")), None, r#""str""#),
            (None, None, "(no status code or body)"),
            (Some(parse(r#"{"message":[1,2.50]}"#)), None, "[1,2.5]"),
        ];
        for (error, message, expected) in cases {
            assert_eq!(sdk_error_message(error.as_ref(), message.as_ref()), expected);
        }
    }

    #[test]
    fn chatgpt_sign_in_detection() {
        let model = Model {
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            ..Default::default()
        };
        assert!(is_chatgpt_sign_in(&model, Some("eyJ.token")));
        assert!(!is_chatgpt_sign_in(&model, Some("sk-abc")));
        assert!(!is_chatgpt_sign_in(&model, None));
    }

    #[test]
    fn service_tier_multipliers() {
        let model = Model {
            id: "gpt-5.5".into(),
            ..Default::default()
        };
        assert_eq!(
            get_service_tier_cost_multiplier(&model, Some("priority")),
            2.5
        );
        assert_eq!(get_service_tier_cost_multiplier(&model, Some("flex")), 0.5);
        assert_eq!(get_service_tier_cost_multiplier(&model, None), 1.0);
    }
}
