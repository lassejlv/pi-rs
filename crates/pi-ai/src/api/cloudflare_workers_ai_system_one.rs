//! System One models on the Workers AI REST endpoint (port of
//! `api/cloudflare-workers-ai-system-one.ts`).
//!
//! `POST /accounts/{account}/ai/run` with `{ model, input }`. The REST API wraps the model
//! output in Cloudflare's API envelope. Third-party models such as `typesafe/jev` add a run
//! record: `{ success, result: { state: "Completed", result: { answers, usage } } }`.
//! Cloudflare-hosted models such as `@cf/cloudflare/clef` return the output directly:
//! `{ success, result: { model, answers, usage } }`.

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::system_one_shared::{SystemOneTransport, SystemOneWireRequest, classify_system_one, join_base_url};
use crate::models::ProviderClassifier;
use crate::types::{ClassifierContext, ClassifierModel, ClassifierResult, StreamOptions};

const LABEL: &str = "Cloudflare Workers AI";

fn cloudflare_error_message(errors: Option<&Value>) -> String {
    if let Some(Value::Array(errors)) = errors {
        let messages: Vec<&str> =
            errors.iter().filter_map(|e| e.as_object().and_then(|o| o.get("message")).and_then(Value::as_str)).collect();
        if !messages.is_empty() {
            return format!("{LABEL} error: {}", messages.join("; "));
        }
    }
    format!("{LABEL} request failed")
}

fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Object(_)) => "[object Object]".to_string(),
        Some(other) => other.to_string(),
    }
}

fn payload(model: &ClassifierModel, request: &SystemOneWireRequest) -> Value {
    json!({ "model": model.id, "input": request.to_value() })
}

fn output(body: Value) -> anyhow::Result<Map<String, Value>> {
    let Value::Object(body) = body else {
        anyhow::bail!("{LABEL} returned an unexpected response");
    };
    if body.get("success") == Some(&Value::Bool(false)) {
        anyhow::bail!("{}", cloudflare_error_message(body.get("errors")));
    }
    let Some(Value::Object(result)) = body.get("result") else {
        anyhow::bail!("{LABEL} returned an unexpected response");
    };
    if result.contains_key("answers") {
        return Ok(result.clone());
    }
    if result.get("state").and_then(Value::as_str) != Some("Completed") {
        anyhow::bail!("{LABEL} run did not complete (state: {})", js_string(result.get("state")));
    }
    match result.get("result") {
        Some(Value::Object(inner)) => Ok(inner.clone()),
        _ => anyhow::bail!("{LABEL} returned an unexpected response"),
    }
}

/// The Workers AI System One transport.
pub const TRANSPORT: SystemOneTransport = SystemOneTransport {
    api: "cloudflare-workers-ai-system-one",
    label: LABEL,
    url: |model| join_base_url(&model.base_url, "run"),
    payload,
    output,
};

/// Cloudflare Workers AI System One classification with public `bool` values mapped to `noul`.
pub async fn classify(model: &ClassifierModel, context: &ClassifierContext, options: StreamOptions) -> ClassifierResult {
    classify_system_one(&TRANSPORT, model, context, &options).await
}

/// The `cloudflare-workers-ai-system-one` classifier API implementation.
pub struct CloudflareWorkersAiSystemOneApi;

#[async_trait]
impl ProviderClassifier for CloudflareWorkersAiSystemOneApi {
    async fn classify(&self, model: &ClassifierModel, context: &ClassifierContext, options: StreamOptions) -> ClassifierResult {
        crate::utils::wasm::assert_send(classify(model, context, options)).await
    }
}
