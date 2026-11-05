//! Host policy defaults. Numeric clock settings stay native numbers, including
//! Infinity, because these policies are runtime values rather than stored JSON.
use crate::*;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock};
pub type SettingsReader = Arc<dyn Fn() -> Result<Settings> + Send + Sync>;
pub fn read_settings(reader: &SettingsReader) -> Result<Settings> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader()))
        .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)))
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSettings {
    pub extensions: Option<Vec<JsonString>>,
    pub stream: Option<JsonObject>,
    pub retry: Option<JsonObject>,
    pub compaction: Option<JsonObject>,
    pub progress: Option<ProgressInput>,
    pub tool_execution: Option<JsonString>,
    pub steering_mode: Option<JsonString>,
    pub follow_up_mode: Option<JsonString>,
    pub context_retention_ms: Option<f64>,
    pub document_retention_ms: Option<f64>,
    pub after_failed_run: Option<JsonString>,
    pub restart_limit: Option<f64>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressInput {
    pub partial_interval_ms: Option<f64>,
    pub output_interval_ms: Option<f64>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressPolicy {
    pub partial_interval_ms: f64,
    pub output_interval_ms: f64,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<JsonString>>,
    pub stream: JsonObject,
    pub retry: JsonObject,
    pub compaction: JsonObject,
    pub progress: ProgressPolicy,
    pub tool_execution: JsonString,
    pub steering_mode: JsonString,
    pub follow_up_mode: JsonString,
    pub context_retention_ms: f64,
    pub document_retention_ms: f64,
    pub after_failed_run: JsonString,
    pub restart_limit: f64,
}
pub const DEFAULT_RESTART_LIMIT: f64 = 5.0;
pub const DEFAULT_PROGRESS_POLICY: ProgressPolicy = ProgressPolicy {
    partial_interval_ms: 100.0,
    output_interval_ms: 100.0,
};
pub static DEFAULT_RETRY_POLICY: LazyLock<JsonObject> = LazyLock::new(|| {
    JsonObject::try_from_value(
        pi_chord::json::parse(
            r#"{"enabled":true,"maxRetries":3,"baseDelayMs":2000,"maxAgentDelayMs":60000}"#,
        )
        .unwrap(),
    )
    .unwrap()
});
pub static DEFAULT_COMPACTION_POLICY: LazyLock<JsonObject> = LazyLock::new(|| {
    JsonObject::try_from_value(pi_chord::json::parse(
        r#"{"enabled":true,"reserveTokens":16384,"keepRecentTokens":20000,"backgroundTokens":32768}"#,
    ).unwrap()).unwrap()
});
pub fn resolve_settings(input: HarnessSettings) -> Settings {
    fn merge(base: &JsonObject, edit: Option<JsonObject>) -> JsonObject {
        let mut base = base.clone();
        if let Some(edit) = edit {
            let JsonValue::Object(edit) = edit.into_value() else {
                unreachable!("JsonObject has an object root")
            };
            base.extend(std::sync::Arc::unwrap_or_clone(edit));
        }
        base
    }
    let progress = input.progress.unwrap_or_default();
    let document_retention = input
        .document_retention_ms
        .filter(|value| !value.is_nan() && *value >= 0.0)
        .unwrap_or_else(crate::session::default_document_retention);
    let restart = input
        .restart_limit
        .filter(|value| *value >= 1.0)
        .map(f64::floor)
        .unwrap_or(DEFAULT_RESTART_LIMIT);
    Settings {
        extensions: input.extensions,
        stream: input.stream.unwrap_or_default(),
        retry: merge(&DEFAULT_RETRY_POLICY, input.retry),
        compaction: merge(&DEFAULT_COMPACTION_POLICY, input.compaction),
        progress: ProgressPolicy {
            partial_interval_ms: progress
                .partial_interval_ms
                .unwrap_or(DEFAULT_PROGRESS_POLICY.partial_interval_ms),
            output_interval_ms: progress
                .output_interval_ms
                .unwrap_or(DEFAULT_PROGRESS_POLICY.output_interval_ms),
        },
        tool_execution: input.tool_execution.unwrap_or_else(|| "parallel".into()),
        steering_mode: input
            .steering_mode
            .unwrap_or_else(|| "one-at-a-time".into()),
        follow_up_mode: input
            .follow_up_mode
            .unwrap_or_else(|| "one-at-a-time".into()),
        context_retention_ms: input.context_retention_ms.unwrap_or(600_000.0),
        document_retention_ms: document_retention,
        after_failed_run: input.after_failed_run.unwrap_or_else(|| "keep".into()),
        restart_limit: restart,
    }
}
