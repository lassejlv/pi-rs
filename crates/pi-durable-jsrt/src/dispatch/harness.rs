//! `harness.*`: native constants and pure helpers of `pi_durable::harness` that the Harness glue
//! exports directly (the stateful Harness service is `transport.*`).

use std::sync::Arc;

use crate::errors::{ErrorClass, HostError};
use crate::host::{Dispatch, Methods};
use crate::value::HostValue;

/// The `harness.*` method group.
pub struct HarnessDispatch;

fn strict(value: &impl serde::Serialize) -> Result<HostValue, HostError> {
    pi_json::strict::to_value(value)
        .map(HostValue::from)
        .map_err(|e| HostError::new(ErrorClass::Error, e))
}

impl Dispatch for HarnessDispatch {
    fn register(&self, m: &mut Methods) {
        // The schema file the native constant is parsed from, in its declared key order.
        m.sync("harness.telemetrySchema", |_, _| {
            let schema = pi_json::strict::parse(include_str!(
                "../../../pi-durable/src/harness/telemetry_schema.json"
            ))
            .map_err(|e| HostError::new(ErrorClass::Error, e))?;
            Ok(HostValue::from(schema))
        });
        m.sync("harness.defaultIdentity", |_, _| {
            strict(&pi_durable::default_harness_identity())
        });
        m.sync("harness.orderToolResults", |_, args| {
            let messages = args.get(0).to_strict_omitting_undefined()?;
            let messages: Vec<Arc<pi_json::strict::Value>> = messages
                .as_array()
                .ok_or_else(|| HostError::type_error("messages must be an array"))?
                .iter()
                .cloned()
                .map(Arc::new)
                .collect();
            let ordered = pi_durable::harness::context::order_tool_results(&messages);
            Ok(HostValue::Array(
                ordered.iter().map(|m| HostValue::from_strict(m)).collect(),
            ))
        });
    }
}
