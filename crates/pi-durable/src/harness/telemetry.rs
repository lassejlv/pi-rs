//! Durable's content-free telemetry vocabulary, shared by instrumentation and adapters.
use pi_telemetry::{TelemetrySchemaDefinition, define_telemetry_schema};
use std::sync::LazyLock;

/// Pinned source metadata. Record IDs are confined to spans and events, never metric labels.
pub static DURABLE_TELEMETRY_SCHEMA: LazyLock<TelemetrySchemaDefinition> = LazyLock::new(|| {
    define_telemetry_schema(
        serde_json::from_str(include_str!("telemetry_schema.json"))
            .expect("valid built-in Durable telemetry schema"),
    )
});
