//! TypeSafe's native System One protocol (port of `api/typesafe-system-one.ts`).
//!
//! OpenRouter serves the same protocol, so both providers use this API with different base URLs.
//!
//! TypeSafe takes images as `assets`. They are experimental and need the
//! `TypeSafe-Danger-Zone-Images` opt-in header; without it the API answers 400. OpenRouter drops
//! `assets` silently and takes images as `image_url` parts in the state instead. Models behind a
//! proxy that hides both the provider ID and the OpenRouter host must set `compat.imageInput`.

use async_trait::async_trait;
use serde_json::{Map, Value};

use super::system_one_shared::{
    ResolvedSystemOneCompat, SystemOneTransport, SystemOneWireRequest, classify_system_one, join_base_url,
};
use crate::models::ProviderClassifier;
use crate::types::{ClassifierContext, ClassifierModel, ClassifierResult, StreamOptions, SystemOneImageInput};

fn payload(model: &ClassifierModel, request: &SystemOneWireRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), Value::String(model.id.clone()));
    body.extend(request.to_map());
    Value::Object(body)
}

/// OpenRouter by provider ID, which survives baseUrl overrides, or by host, which covers regional
/// endpoints.
fn detect_compat(model: &ClassifierModel) -> ResolvedSystemOneCompat {
    if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
        ResolvedSystemOneCompat { image_input: SystemOneImageInput::StateImageUrls, send_image_opt_in_header: false }
    } else {
        ResolvedSystemOneCompat { image_input: SystemOneImageInput::Assets, send_image_opt_in_header: true }
    }
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
    detect_compat,
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
