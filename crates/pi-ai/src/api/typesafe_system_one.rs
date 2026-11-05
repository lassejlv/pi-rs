//! TypeSafe's native System One protocol (port of `api/typesafe-system-one.ts`).
//!
//! OpenRouter serves the same protocol, so both providers use this API with different base URLs.

use async_trait::async_trait;
use serde_json::{Map, Value};

use super::system_one_shared::{SystemOneTransport, SystemOneWireRequest, classify_system_one, join_base_url};
use crate::models::ProviderClassifier;
use crate::types::{ClassifierContext, ClassifierModel, ClassifierResult, StreamOptions};

fn payload(model: &ClassifierModel, request: &SystemOneWireRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), Value::String(model.id.clone()));
    body.insert("state".into(), Value::Object(request.state.clone()));
    body.insert("questions".into(), Value::Object(request.questions.clone()));
    Value::Object(body)
}

fn output(body: Value) -> anyhow::Result<Map<String, Value>> {
    match body {
        Value::Object(map) => Ok(map),
        _ => anyhow::bail!("System One API returned an unexpected response"),
    }
}

/// The TypeSafe System One transport.
pub const TRANSPORT: SystemOneTransport = SystemOneTransport {
    api: "typesafe-system-one",
    label: "System One API",
    url: |model| join_base_url(&model.base_url, "systemone"),
    payload,
    output,
};

/// TypeSafe System One classification with public `bool` values mapped to wire-level `noul`.
pub async fn classify(model: &ClassifierModel, context: &ClassifierContext, options: StreamOptions) -> ClassifierResult {
    classify_system_one(&TRANSPORT, model, context, &options).await
}

/// The `typesafe-system-one` classifier API implementation.
pub struct TypesafeSystemOneApi;

#[async_trait]
impl ProviderClassifier for TypesafeSystemOneApi {
    async fn classify(&self, model: &ClassifierModel, context: &ClassifierContext, options: StreamOptions) -> ClassifierResult {
        crate::utils::wasm::assert_send(classify(model, context, options)).await
    }
}
