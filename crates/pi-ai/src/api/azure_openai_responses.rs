//! Azure OpenAI Responses API (port of `api/azure-openai-responses.ts`).
//!
//! Requests mirror the `AzureOpenAI` client of the `openai` JS SDK: `POST
//! {baseUrl}/responses?api-version={apiVersion}` with an `api-key` header.
//!
//! API-specific options are read from `StreamOptions.extra`: `reasoningEffort`,
//! `reasoningSummary`, `toolChoice`, plus the Azure endpoint options of
//! [`azure_openai_config`](super::azure_openai_config).

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::azure_openai_config::{resolve_azure_config, resolve_deployment_name};
use super::constrained_sampling::create_grammar_tool_input_properties;
use super::openai_prompt_cache::clamp_openai_prompt_cache_key;
use super::openai_responses::{
    OPENAI_RESPONSES_MIN_OUTPUT_TOKENS, error_message_of, mapped_thinking_level,
    openai_sdk_base_headers, openai_sse_events, simple_reasoning_effort, thinking_level_entry,
};
use super::openai_responses_shared::{
    ConvertResponsesMessagesOptions, ConvertResponsesToolsOptions, OpenAIResponsesStreamOptions,
    convert_responses_messages, convert_responses_tools, get_responses_tool_support,
    process_responses_stream,
};
use super::simple_options::{build_base_options, resolve_sampling_params};
use crate::models::ProviderStreams;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Model, ModelThinkingLevel, ProviderResponse,
    StopReason, StreamOptions, TranscriptContext,
};
use crate::utils::error_body::SdkErrorStyle;
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};
use crate::utils::headers::headers_to_record;
use crate::utils::http::{HeaderList, apply_openai_custom_headers_env, client_for, post_json};
use crate::utils::identity::{ClientIdentityTarget, get_identity_headers};
use crate::utils::provider_retry::{ProviderRetryOptions, retry_provider_request};
use crate::utils::transcript::{get_declared_tools, resolve_transcript, resolve_transcript_tools};

const AZURE_TOOL_CALL_PROVIDERS: [&str; 4] = ["openai", "openai-codex", "opencode", "azure"];
const DEFAULT_TIMEOUT_MS: u64 = 600_000;

fn build_params(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    deployment_name: &str,
    grammar_tool_input_properties: &HashMap<String, String>,
) -> anyhow::Result<Value> {
    let compat = model.compat();
    let supports_mid_convo = compat.supports_mid_convo_system_messages.unwrap_or(false);
    let supports_additional_tools = compat.supports_additional_tools.unwrap_or(false);
    let supports_tool_search = compat.supports_tool_search.unwrap_or(false);
    let tool_options = ConvertResponsesToolsOptions {
        supports_strict_mode: Some(compat.supports_strict_mode.unwrap_or(true)),
        supports_openai_grammar_tools: compat.supports_openai_grammar_tools.unwrap_or(false),
        ..Default::default()
    };
    let transcript_tools = resolve_transcript_tools(
        &context.messages,
        get_responses_tool_support(
            supports_mid_convo,
            supports_additional_tools,
            supports_tool_search,
        ),
    );
    let messages = convert_responses_messages(
        model,
        context,
        &AZURE_TOOL_CALL_PROVIDERS,
        &ConvertResponsesMessagesOptions {
            grammar_tool_input_properties: Some(grammar_tool_input_properties),
            supports_mid_convo_system_messages: supports_mid_convo,
            supports_additional_tools,
            supports_tool_search,
            tool_options: tool_options.clone(),
            ..Default::default()
        },
    )?;

    let mut params = Map::new();
    params.insert("model".into(), json!(deployment_name));
    params.insert("input".into(), Value::Array(messages));
    params.insert("stream".into(), json!(true));
    if let Some(key) = clamp_openai_prompt_cache_key(options.session_id.as_deref()) {
        params.insert("prompt_cache_key".into(), json!(key));
    }
    params.insert("store".into(), json!(false));
    if let Some(max_tokens) = options.max_tokens.filter(|m| *m > 0) {
        params.insert(
            "max_output_tokens".into(),
            json!(max_tokens.max(OPENAI_RESPONSES_MIN_OUTPUT_TOKENS)),
        );
    }
    if let Some(temperature) = options.temperature {
        params.insert(
            "temperature".into(),
            crate::serde_util::js_number(temperature),
        );
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
    // `toolChoice !== undefined`: an explicit null is sent.
    if let Some(choice) = options.extra_value("toolChoice") {
        params.insert("tool_choice".into(), choice.clone());
    }

    let requested_effort = options
        .extra_str("reasoningEffort")
        .filter(|s| !s.is_empty());
    let reasoning_summary = options
        .extra_str("reasoningSummary")
        .filter(|s| !s.is_empty());
    let reasoning_effort = requested_effort
        .map(str::to_string)
        .or_else(|| reasoning_summary.map(|_| "medium".to_string()));
    if model.reasoning {
        if let Some(effort) = &reasoning_effort {
            let effort = match requested_effort {
                Some(requested) => mapped_thinking_level(model, requested),
                None => effort.clone(),
            };
            params.insert(
                "reasoning".into(),
                json!({ "effort": effort, "summary": reasoning_summary.unwrap_or("auto") }),
            );
            params.insert("include".into(), json!(["reasoning.encrypted_content"]));
        } else {
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

fn build_headers(
    model: &Model,
    api_key: &str,
    options: &StreamOptions,
    timeout_ms: u64,
) -> anyhow::Result<HeaderList> {
    let mut headers = openai_sdk_base_headers(timeout_ms);
    headers.set("api-key", Some(api_key));
    apply_openai_custom_headers_env(&mut headers)?;
    let target = ClientIdentityTarget {
        provider: &model.provider,
        base_url: &model.base_url,
    };
    headers.extend_strings(&get_identity_headers(target, options.identity.as_ref()));
    if let Some(model_headers) = &model.headers {
        headers.extend_strings(model_headers);
    }
    if let Some(h) = &options.headers {
        headers.extend_optional(h);
    }
    headers.set("Content-Type", Some("application/json"));
    Ok(headers)
}

async fn run_stream(
    model: Model,
    context: TranscriptContext,
    options: StreamOptions,
    sender: AssistantMessageEventSender,
) {
    let compat = model.compat();
    let normalized = resolve_transcript(&context, compat.supports_mid_convo_system_messages);
    let deployment_name = resolve_deployment_name(&model, Some(&options));
    let mut output = AssistantMessage {
        api: "azure-openai-responses".into(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        stop_reason: StopReason::Pending,
        timestamp: crate::utils::now_ms(),
        ..Default::default()
    };
    let signal = options.signal.clone();
    let aborted = || signal.as_ref().is_some_and(|s| s.is_cancelled());

    let result: anyhow::Result<()> = async {
        let Some(api_key) = options.api_key.clone().filter(|k| !k.is_empty()) else {
            anyhow::bail!("No API key for provider: {}", model.provider);
        };
        let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let (base_url, api_version) = resolve_azure_config(&model, Some(&options))?;
        let headers = build_headers(&model, &api_key, &options, timeout_ms)?;
        let grammar_props = create_grammar_tool_input_properties(
            &get_declared_tools(&normalized.messages),
            compat.supports_openai_grammar_tools.unwrap_or(false),
        )?;
        let mut params = build_params(
            &model,
            &normalized,
            &options,
            &deployment_name,
            &grammar_props,
        )?;
        if let Some(on_payload) = &options.on_payload
            && let Some(next) = on_payload(params.clone(), model.clone()).await
        {
            params = next;
        }
        let mut url = url::Url::parse(&format!("{}/responses", base_url.trim_end_matches('/')))?;
        url.query_pairs_mut()
            .append_pair("api-version", &api_version);
        let url = url.to_string();
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
        process_responses_stream(
            openai_sse_events(response, signal.clone()),
            &mut output,
            &sender,
            &model,
            OpenAIResponsesStreamOptions {
                on_provider_stream_event: options.on_provider_stream_event.clone(),
                grammar_tool_input_properties: Some(&grammar_props),
                ..Default::default()
            },
        )
        .await?;
        if aborted() {
            anyhow::bail!("Request was aborted");
        }
        match output.stop_reason {
            StopReason::Pending => {
                anyhow::bail!("Azure OpenAI Responses stream ended without a stop reason")
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
        output.error_message = Some(error_message_of(&error, Some("Azure OpenAI API error")));
        sender.push(AssistantMessageEvent::Error {
            reason: output.stop_reason,
            error: output.clone(),
        });
        sender.end(None);
    }
}

/// Stream an Azure OpenAI Responses response.
pub fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    let (sender, stream) = AssistantMessageEventStream::new();
    crate::utils::wasm::spawn(run_stream(model.clone(), context.clone(), options, sender));
    stream
}

/// Stream with provider-neutral reasoning options.
pub fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    let Some(api_key) = options.api_key.clone().filter(|k| !k.is_empty()) else {
        return super::lazy::error_stream(
            model,
            anyhow::anyhow!("No API key for provider: {}", model.provider),
        );
    };
    let mut base = build_base_options(model, context, &options, Some(&api_key));
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

/// The `azure-openai-responses` API implementation.
pub struct AzureOpenAIResponsesApi;

impl ProviderStreams for AzureOpenAIResponsesApi {
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
