//! Image generation over OpenRouter's chat completions endpoint (port of
//! `api/openrouter-images.ts`).
//!
//! Requests mirror what the `openai` SDK sends for `chat.completions.create(...)`: a non-streaming
//! `POST {baseUrl}/chat/completions` with bearer auth and the Stainless headers.

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};

use super::classifier_shared::image_hook_model;
use crate::models::ProviderImages;
use crate::types::{
    AssistantImages, ImageContent, ImageModel, ImagesContext, ImagesStopReason, ProviderResponse, StreamOptions,
    TextContent, Usage, UsageCost, UserContentBlock,
};
use crate::utils::error_body::{SdkErrorStyle, format_provider_error, normalize_provider_error};
use crate::utils::headers::{headers_to_record, provider_headers_to_record, to_provider_headers};
use crate::utils::http::{HeaderList, client_for, post_json, stainless_headers};
use crate::utils::identity::{ClientIdentityTarget, get_identity_headers};
use crate::utils::provider_retry::{AbortError, ProviderRetryOptions, retry_provider_request};
use crate::utils::sanitize_unicode::sanitize_surrogates;

/// Version of the `openai` SDK whose request shape this module reproduces.
pub const OPENAI_SDK_VERSION: &str = "7.19.0";
const DEFAULT_TIMEOUT_MS: u64 = 600_000;

fn build_params(model: &ImageModel, context: &ImagesContext) -> Value {
    let content: Vec<Value> = context
        .input
        .iter()
        .map(|item| match item {
            UserContentBlock::Text(text) => json!({ "type": "text", "text": sanitize_surrogates(&text.text) }),
            UserContentBlock::Image(image) => json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{}", image.mime_type, image.data) }
            }),
        })
        .collect();
    let modalities = if model.output.iter().any(|o| o == "text") { json!(["image", "text"]) } else { json!(["image"]) };
    json!({
        "model": model.id,
        "messages": [{ "role": "user", "content": content }],
        "stream": false,
        "modalities": modalities,
    })
}

fn tokens(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_f64).filter(|v| v.is_finite() && *v > 0.0).map(|v| v as u64).unwrap_or(0)
}

fn parse_usage(raw: &Value, model: &ImageModel) -> Usage {
    let prompt_tokens = tokens(raw.get("prompt_tokens"));
    let details = raw.get("prompt_tokens_details");
    let reported_cached = tokens(details.and_then(|d| d.get("cached_tokens")));
    let cache_write = tokens(details.and_then(|d| d.get("cache_write_tokens")));
    let cache_read = if cache_write > 0 { reported_cached.saturating_sub(cache_write) } else { reported_cached };
    let input = prompt_tokens.saturating_sub(cache_read).saturating_sub(cache_write);
    let output = tokens(raw.get("completion_tokens"));
    let mut cost = UsageCost {
        input: (model.cost.input / 1_000_000.0) * input as f64,
        output: (model.cost.output / 1_000_000.0) * output as f64,
        cache_read: (model.cost.cache_read / 1_000_000.0) * cache_read as f64,
        cache_write: (model.cost.cache_write / 1_000_000.0) * cache_write as f64,
        total: 0.0,
    };
    cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: input + output + cache_read + cache_write,
        cost,
        ..Default::default()
    }
}

fn build_headers(model: &ImageModel, api_key: &str, options: &StreamOptions, timeout_ms: u64) -> anyhow::Result<HeaderList> {
    let mut headers = HeaderList::new();
    headers.set("Accept", Some("application/json"));
    headers.set("User-Agent", Some(&format!("OpenAI/JS {OPENAI_SDK_VERSION}")));
    for (name, value) in stainless_headers(OPENAI_SDK_VERSION, timeout_ms) {
        headers.set(&name, Some(&value));
    }
    crate::utils::http::apply_openai_sdk_tenant_headers(&mut headers);
    headers.set("Authorization", Some(&format!("Bearer {api_key}")));
    crate::utils::http::apply_openai_custom_headers_env(&mut headers)?;
    let identity = to_provider_headers(&get_identity_headers(
        ClientIdentityTarget { provider: &model.provider, base_url: &model.base_url },
        options.identity.as_ref(),
    ));
    let model_headers = model.headers.as_ref().map(to_provider_headers);
    if let Some(defaults) = provider_headers_to_record(&[Some(&identity), model_headers.as_ref(), options.headers.as_ref()]) {
        headers.extend_strings(&defaults);
    }
    if let Some(explicit) = &options.headers {
        for (name, value) in explicit {
            if value.is_none() {
                headers.set(name, None);
            }
        }
    }
    headers.set("Content-Type", Some("application/json"));
    Ok(headers)
}

/// Image generation over OpenRouter's chat completions endpoint. Never fails: errors become
/// results.
pub async fn generate_images(model: &ImageModel, context: &ImagesContext, options: StreamOptions) -> AssistantImages {
    let mut output = AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: Vec::new(),
        response_id: None,
        usage: None,
        stop_reason: ImagesStopReason::Stop,
        error_message: None,
        timestamp: crate::utils::now_ms(),
    };
    let result: anyhow::Result<()> = async {
        let Some(api_key) = options.api_key.clone().filter(|k| !k.is_empty()) else {
            anyhow::bail!("No API key for provider: {}", model.provider);
        };
        let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        // `createClient` (which can reject OPENAI_CUSTOM_HEADERS) runs before the params.
        let headers = build_headers(model, &api_key, &options, timeout_ms)?;
        let mut params = build_params(model, context);
        if let Some(on_payload) = &options.on_payload
            && let Some(next) = on_payload(params.clone(), image_hook_model(model)).await {
                params = next;
            }
        let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
        let client = match &options.http_client {
            Some(client) => client.clone(),
            None => client_for(&url, options.env.as_ref())?,
        };
        if options.is_aborted() {
            anyhow::bail!(AbortError);
        }
        let retry = ProviderRetryOptions {
            max_retries: options.max_retries,
            max_retry_delay_ms: options.max_retry_delay_ms,
            signal: options.signal.clone(),
            no_retry_statuses: Vec::new(),
        };
        let response = retry_provider_request(
            || post_json(&client, &url, &headers, &params, Some(timeout_ms), options.signal.as_ref(), SdkErrorStyle::OpenAI),
            &retry,
        )
        .await?;
        if let Some(on_response) = &options.on_response {
            on_response(
                ProviderResponse { status: response.status().as_u16(), headers: headers_to_record(response.headers()) },
                image_hook_model(model),
            )
            .await;
        }
        let body: Value = response.json().await?;
        output.response_id = body.get("id").and_then(Value::as_str).map(str::to_string);
        if let Some(usage) = body.get("usage").filter(|u| u.is_object()) {
            output.usage = Some(parse_usage(usage, model));
        }
        if let Some(choice) = body.get("choices").and_then(|c| c.get(0)) {
            let message = &choice["message"];
            if let Some(text) = message.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                output.output.push(UserContentBlock::Text(TextContent::new(text)));
            }
            let data_url = Regex::new(r"^data:([^;]+);base64,(.+)$").expect("valid regex");
            for image in message.get("images").and_then(Value::as_array).into_iter().flatten() {
                let url = match image.get("image_url") {
                    Some(Value::String(url)) => Some(url.as_str()),
                    Some(Value::Object(obj)) => obj.get("url").and_then(Value::as_str),
                    _ => None,
                };
                let Some(url) = url.filter(|u| u.starts_with("data:")) else { continue };
                let Some(captures) = data_url.captures(url) else { continue };
                output.output.push(UserContentBlock::Image(ImageContent {
                    mime_type: captures[1].to_string(),
                    data: captures[2].to_string(),
                }));
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        output.stop_reason = if options.is_aborted() { ImagesStopReason::Aborted } else { ImagesStopReason::Error };
        output.error_message = Some(format_provider_error(&normalize_provider_error(&error), None));
    }
    output
}

/// The `openrouter-images` image API implementation.
pub struct OpenRouterImagesApi;

#[async_trait]
impl ProviderImages for OpenRouterImagesApi {
    async fn generate_images(&self, model: &ImageModel, context: &ImagesContext, options: StreamOptions) -> AssistantImages {
        crate::utils::wasm::assert_send(generate_images(model, context, options)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_splits_cache_reads_and_writes() {
        let model = ImageModel {
            cost: crate::types::ModelCost { input: 1.0, output: 2.0, cache_read: 0.5, cache_write: 4.0, ..Default::default() },
            ..Default::default()
        };
        let usage = parse_usage(
            &json!({ "prompt_tokens": 100, "completion_tokens": 10, "prompt_tokens_details": { "cached_tokens": 30, "cache_write_tokens": 20 } }),
            &model,
        );
        assert_eq!((usage.input, usage.output, usage.cache_read, usage.cache_write, usage.total_tokens), (70, 10, 10, 20, 110));
        assert!((usage.cost.total - (70.0 + 20.0 + 5.0 + 80.0) / 1_000_000.0).abs() < 1e-12);
    }
}
