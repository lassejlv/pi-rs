//! Port of `packages/coding-agent/src/core/keybindings-schema.ts`.
//!
//! The TS schema has one optional property per `KEYBINDINGS` id (with its description) and
//! `additionalProperties: KeybindingValueSchema`. It is rebuilt here from the [`KEYBINDINGS`]
//! table so ids and descriptions stay in sync.

use crate::core::key_id_schema::keybinding_value_schema;
use crate::core::keybindings::KEYBINDINGS;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

pub use crate::core::key_id_schema::KEYBINDING_VALUE_SCHEMA;

/// The published `keybindings.schema.json` artifact of the TS package.
pub const KEYBINDINGS_SCHEMA_ARTIFACT: pi_embed::Embedded = pi_embed::include_str!("schemas/keybindings.schema.json");

pub static KEYBINDINGS_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    let mut properties = Map::new();
    properties.insert("$schema".to_string(), json!({"type": "string"}));
    for (id, definition) in KEYBINDINGS.iter() {
        let mut options = Map::new();
        if let Some(description) = &definition.description {
            options.insert("description".to_string(), Value::String(description.clone()));
        }
        properties.insert(id.clone(), keybinding_value_schema(Some(Value::Object(options))));
    }
    json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": KEYBINDING_VALUE_SCHEMA.clone(),
    })
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings_schema::strip_generated_schema;

    #[test]
    fn matches_the_generated_artifact() {
        assert_eq!(
            *KEYBINDINGS_SCHEMA,
            strip_generated_schema(KEYBINDINGS_SCHEMA_ARTIFACT.get())
        );
    }
}
