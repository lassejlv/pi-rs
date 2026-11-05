//! Google Generative AI (Gemini API) (port of `api/google-generative-ai.ts`).
//!
//! Implemented over HTTP the way `@google/genai` talks to the Gemini API:
//! `POST {baseUrl}/{model}:streamGenerateContent?alt=sse` with `x-goog-api-key`.
//!
//! API-specific options are read from `StreamOptions.extra`: `toolChoice` (`auto`/`none`/`any`)
//! and `thinking` (`{ enabled, budgetTokens?, level? }`). The `onPayload` hook receives the SDK's
//! `GenerateContentParameters` shape (`{ model, contents, config }`).

use serde_json::Value;

use super::google_shared::{
    GoogleClient, GoogleClientOptions, GoogleStreamSpec, google_http_headers, google_tool_choice, run_google_stream,
    simple_thinking_options,
};
use super::simple_options::build_base_options;
use crate::models::ProviderStreams;
use crate::types::{AssistantMessageEventStream, Model, StreamOptions, TranscriptContext};

fn create_client(model: &Model, options: &StreamOptions) -> anyhow::Result<GoogleClient> {
    let Some(api_key) = options.api_key.clone().filter(|k| !k.is_empty()) else {
        anyhow::bail!("No API key for provider: {}", model.provider);
    };
    let has_base_url = !model.base_url.is_empty();
    GoogleClient::new(GoogleClientOptions {
        vertexai: false,
        api_key: Some(api_key),
        base_url: has_base_url.then(|| model.base_url.clone()),
        // The base URL already includes the version path.
        http_api_version: has_base_url.then(String::new),
        headers: google_http_headers(model, options),
        ..Default::default()
    })
}

/// Stream a Gemini response.
pub fn stream(model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
    let (sender, stream) = AssistantMessageEventStream::new();
    let spec = GoogleStreamSpec {
        label: "Google",
        create_client: Box::new(create_client),
        custom_client_error: "Custom fetch is not supported by the Google Generative AI adapter",
    };
    crate::utils::wasm::spawn(run_google_stream(model.clone(), context.clone(), options, sender, spec));
    stream
}

/// Stream with provider-neutral reasoning options.
pub fn stream_simple(model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
    let Some(api_key) = options.api_key.clone().filter(|k| !k.is_empty()) else {
        return super::lazy::error_stream(model, anyhow::anyhow!("No API key for provider: {}", model.provider));
    };
    let mut base = build_base_options(model, context, &options, Some(&api_key));
    if let Some(choice) = google_tool_choice(&options) {
        base.extra.insert("toolChoice".into(), Value::String(choice));
    }
    let thinking = match simple_thinking_options(model, &options, true) {
        Ok(thinking) => thinking,
        Err(error) => return super::lazy::error_stream(model, error),
    };
    stream(model, context, base.with_extra("thinking", thinking.to_value()))
}

/// The `google-generative-ai` API implementation.
pub struct GoogleGenerativeAiApi;

impl ProviderStreams for GoogleGenerativeAiApi {
    fn stream(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        stream(model, context, options)
    }

    fn stream_simple(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        stream_simple(model, context, options)
    }
}
