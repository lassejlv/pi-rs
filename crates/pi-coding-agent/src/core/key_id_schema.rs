//! Port of `packages/coding-agent/src/core/key-id-schema.ts`.
//!
//! The schemas are taken from the generated `keybindings.schema.json` artifact (its
//! `KeybindingValue` definition holds the key-id pattern built from the pi-tui `Key` names).

use crate::core::keybindings_schema::KEYBINDINGS_SCHEMA_ARTIFACT;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

static KEYBINDING_VALUE_DEF: LazyLock<Value> = LazyLock::new(|| {
    let artifact: Value =
        serde_json::from_str(KEYBINDINGS_SCHEMA_ARTIFACT.get()).expect("valid keybindings schema");
    artifact["$defs"]["KeybindingValue"].clone()
});

/// Key identifier with optional ctrl, shift, alt, or super modifiers.
pub static KEY_ID_SCHEMA: LazyLock<Value> =
    LazyLock::new(|| KEYBINDING_VALUE_DEF["anyOf"][0].clone());

/// `Type.Union([KeyIdSchema, Type.Array(KeyIdSchema)], options)`.
pub fn keybinding_value_schema(options: Option<Value>) -> Value {
    let mut schema = Map::new();
    schema.insert(
        "anyOf".to_string(),
        json!([KEY_ID_SCHEMA.clone(), {"type": "array", "items": KEY_ID_SCHEMA.clone()}]),
    );
    if let Some(Value::Object(options)) = options {
        for (key, value) in options {
            schema.insert(key, value);
        }
    }
    Value::Object(schema)
}

pub static KEYBINDING_VALUE_SCHEMA: LazyLock<Value> =
    LazyLock::new(|| keybinding_value_schema(None));

/// The key-id pattern split into its duplicate-modifier lookahead and the main expression (the
/// `regex` crate has no lookaround).
static KEY_ID_REGEXES: LazyLock<(regex::Regex, regex::Regex)> = LazyLock::new(|| {
    let pattern = KEY_ID_SCHEMA["pattern"].as_str().expect("key id pattern");
    let rest = pattern
        .strip_prefix("^(?!.*(?:")
        .expect("key id pattern starts with the duplicate-modifier lookahead");
    let end = rest.find("))").expect("end of lookahead");
    let duplicate = regex::Regex::new(&rest[..end]).expect("valid duplicate-modifier regex");
    let main = regex::Regex::new(&format!("^{}", &rest[end + 2..])).expect("valid key id regex");
    (duplicate, main)
});

/// Whether `value` matches [`KEY_ID_SCHEMA`].
pub fn is_valid_key_id(value: &str) -> bool {
    let (duplicate, main) = &*KEY_ID_REGEXES;
    !duplicate.is_match(value) && main.is_match(value)
}

/// `Compile(KeybindingValueSchema).Check(value)`.
pub fn is_valid_keybinding_value(value: &Value) -> bool {
    match value {
        Value::String(s) => is_valid_key_id(s),
        Value::Array(items) => items
            .iter()
            .all(|item| item.as_str().is_some_and(is_valid_key_id)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_keybinding_syntax() {
        for binding in [
            "a",
            "9",
            "pageUp",
            "+",
            "ctrl+shift+x",
            "alt+ctrl+?",
            "ctrl+shift+alt+super+f12",
        ] {
            assert!(is_valid_key_id(binding), "{binding}");
        }
        for binding in [
            "",
            "Ctrl+x",
            "control+x",
            "ctrl+not-a-key",
            "ctrl+ctrl+x",
            "ctrl+shift+alt+super+ctrl+x",
        ] {
            assert!(!is_valid_key_id(binding), "{binding}");
        }
        assert!(is_valid_keybinding_value(&json!(["alt+x", "ctrl+y"])));
        assert!(is_valid_keybinding_value(&json!([])));
        assert!(!is_valid_keybinding_value(&json!(42)));
        assert!(!is_valid_keybinding_value(&json!(["alt+x", 1])));
    }

    #[test]
    fn value_schema_carries_options() {
        let schema = keybinding_value_schema(Some(json!({"description": "Undo"})));
        assert_eq!(schema["description"], "Undo");
        assert_eq!(schema["anyOf"][1]["type"], "array");
    }
}
