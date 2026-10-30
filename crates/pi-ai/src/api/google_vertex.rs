//! Google Vertex AI (port of `api/google-vertex.ts`).
//!
//! Implemented over HTTP the way `@google/genai` talks to Vertex AI (`apiVersion: "v1"`): either
//! a Vertex API key (`x-goog-api-key`, global endpoint) or Application Default Credentials with a
//! project and location (`Authorization: Bearer`, regional endpoint, see [`super::google_auth`]).
//!
//! API-specific options are read from `StreamOptions.extra`: `toolChoice`, `thinking`
//! (`{ enabled, budgetTokens?, level? }`), `project` and `location`.

use serde_json::Value;

use super::google_shared::{
    GoogleClient, GoogleClientOptions, GoogleStreamSpec, google_http_headers, google_tool_choice, run_google_stream,
    simple_thinking_options,
};
use super::simple_options::build_base_options;
use crate::models::ProviderStreams;
use crate::types::{AssistantMessageEventStream, Model, StreamOptions, TranscriptContext};
use crate::utils::provider_env::get_provider_env_value;

const API_VERSION: &str = "v1";
const GCP_VERTEX_CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";

fn is_placeholder_api_key(api_key: &str) -> bool {
    api_key.len() > 2 && api_key.starts_with('<') && api_key.ends_with('>') && !api_key[1..api_key.len() - 1].contains('>')
}

/// The Vertex API key, ignoring the ADC markers (`gcp-vertex-credentials`, `<authenticated>`).
pub fn resolve_api_key(options: &StreamOptions) -> Option<String> {
    let api_key = options.api_key.as_deref()?.trim();
    if api_key.is_empty() || api_key == GCP_VERTEX_CREDENTIALS_MARKER || is_placeholder_api_key(api_key) {
        return None;
    }
    Some(api_key.to_string())
}

fn resolve_project(options: &StreamOptions) -> anyhow::Result<String> {
    let project = options
        .extra_str("project")
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .or_else(|| get_provider_env_value("GOOGLE_CLOUD_PROJECT", options.env.as_ref()))
        .or_else(|| get_provider_env_value("GCLOUD_PROJECT", options.env.as_ref()));
    project.ok_or_else(|| {
        anyhow::anyhow!("Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options.")
    })
}

fn resolve_location(options: &StreamOptions) -> anyhow::Result<String> {
    let location = options
        .extra_str("location")
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .or_else(|| get_provider_env_value("GOOGLE_CLOUD_LOCATION", options.env.as_ref()));
    location.ok_or_else(|| anyhow::anyhow!("Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options."))
}

fn resolve_custom_base_url(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() || trimmed.contains("{location}") {
        return None;
    }
    Some(trimmed.to_string())
}

/// Whether a custom base URL already names an API version path segment (`v1`, `v1beta1`, ...).
pub fn base_url_includes_api_version(base_url: &str) -> bool {
    let version = regex::Regex::new(r"^v\d+(?:beta\d*)?$").expect("valid regex");
    match url::Url::parse(base_url) {
        Ok(url) => url.path().split('/').any(|part| version.is_match(part)),
        Err(_) => regex::Regex::new(r"(?:^|/)v\d+(?:beta\d*)?(?:/|$)").expect("valid regex").is_match(base_url),
    }
}

/// The `GoogleGenAI` options the TypeScript adapter passes for this request.
pub fn client_options(model: &Model, options: &StreamOptions) -> anyhow::Result<GoogleClientOptions> {
    let base_url = resolve_custom_base_url(&model.base_url);
    let http_api_version = base_url.as_deref().filter(|u| base_url_includes_api_version(u)).map(|_| String::new());
    let mut client = GoogleClientOptions {
        vertexai: true,
        api_version: Some(API_VERSION.into()),
        base_url_collection_scope: base_url.is_some(),
        base_url,
        http_api_version,
        headers: google_http_headers(model, options),
        ..Default::default()
    };
    match resolve_api_key(options) {
        Some(api_key) => client.api_key = Some(api_key),
        None => {
            client.project = Some(resolve_project(options)?);
            client.location = Some(resolve_location(options)?);
            client.key_filename = get_provider_env_value("GOOGLE_APPLICATION_CREDENTIALS", options.env.as_ref());
        }
    }
    Ok(client)
}

fn create_client(model: &Model, options: &StreamOptions) -> anyhow::Result<GoogleClient> {
    GoogleClient::new(client_options(model, options)?)
}

/// Stream a Vertex AI response.
pub fn stream(model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
    let (sender, stream) = AssistantMessageEventStream::new();
    let spec = GoogleStreamSpec {
        label: "Google Vertex",
        create_client: Box::new(create_client),
        custom_client_error: "Custom fetch is not supported by the Google Vertex adapter",
    };
    crate::utils::wasm::spawn(run_google_stream(model.clone(), context.clone(), options, sender, spec));
    stream
}

/// Stream with provider-neutral reasoning options.
pub fn stream_simple(model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
    let mut base = build_base_options(model, context, &options, None);
    for key in ["project", "location"] {
        if let Some(value) = options.extra_value(key) {
            base.extra.insert(key.into(), value.clone());
        }
    }
    if let Some(choice) = google_tool_choice(&options) {
        base.extra.insert("toolChoice".into(), Value::String(choice));
    }
    let thinking = match simple_thinking_options(model, &options, false) {
        Ok(thinking) => thinking,
        Err(error) => return super::lazy::error_stream(model, error),
    };
    stream(model, context, base.with_extra("thinking", thinking.to_value()))
}

/// The `google-vertex` API implementation.
pub struct GoogleVertexApi;

impl ProviderStreams for GoogleVertexApi {
    fn stream(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        stream(model, context, options)
    }

    fn stream_simple(&self, model: &Model, context: &TranscriptContext, options: StreamOptions) -> AssistantMessageEventStream {
        stream_simple(model, context, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_markers() {
        let with = |key: &str| StreamOptions { api_key: Some(key.into()), ..Default::default() };
        assert_eq!(resolve_api_key(&with("<authenticated>")), None);
        assert_eq!(resolve_api_key(&with("gcp-vertex-credentials")), None);
        assert_eq!(resolve_api_key(&with(" AIza ")).as_deref(), Some("AIza"));
    }

    #[test]
    fn api_version_detection() {
        assert!(base_url_includes_api_version("https://proxy.example.com/v1/projects/p/locations/global"));
        assert!(base_url_includes_api_version("https://proxy.example.com/v1beta1"));
        assert!(!base_url_includes_api_version("https://proxy.example.com"));
    }
}
