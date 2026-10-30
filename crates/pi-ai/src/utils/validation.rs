//! Tool argument validation and coercion (port of `utils/validation.ts`).
//!
//! TypeBox schemas are JSON Schema. This module contains a JSON Schema validator that
//! mirrors TypeBox's checker (keyword order, English error messages and instance paths),
//! plus the AJV-compatible primitive coercion pi applies to plain JSON schemas.
//!
//! In Rust every schema is plain JSON, so the plain-schema path of `validateToolArguments`
//! (`normalizeOptionalNulls` + `coerceWithJsonSchema`) applies to all tools.

use pi_json::strict;
use serde_json::Value;

use crate::types::{Tool, ToolCall};

pub mod utf16;

fn to_strict(value: &Value) -> strict::Value {
    strict::Value::from(value.clone())
}

/// Every string here began as UTF-8, so the conversion back is exact.
fn to_serde(value: strict::Value) -> Value {
    match value {
        strict::Value::Null => Value::Null,
        strict::Value::Bool(b) => Value::Bool(b),
        strict::Value::Number(n) => Value::Number(n),
        strict::Value::String(s) => Value::String(s.to_string_lossy()),
        strict::Value::Array(items) => Value::Array(
            std::sync::Arc::unwrap_or_clone(items)
                .into_iter()
                .map(to_serde)
                .collect(),
        ),
        strict::Value::Object(map) => Value::Object(
            std::sync::Arc::unwrap_or_clone(map)
                .into_iter()
                .map(|(k, v)| (k.to_string_lossy(), to_serde(v)))
                .collect(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Validator
// ---------------------------------------------------------------------------

/// A validation error in TypeBox's shape.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub keyword: String,
    pub schema_path: String,
    pub instance_path: String,
    /// For `required`: the missing properties.
    pub required_properties: Vec<String>,
    pub message: String,
}

impl From<utf16::ValidationError> for ValidationError {
    fn from(error: utf16::ValidationError) -> Self {
        Self {
            keyword: error.keyword,
            schema_path: error.schema_path.to_string_lossy(),
            instance_path: error.instance_path.to_string_lossy(),
            required_properties: error
                .required_properties
                .iter()
                .map(strict::JsonString::to_string_lossy)
                .collect(),
            message: error.message.to_string_lossy(),
        }
    }
}

/// A compiled JSON Schema checker (TypeBox's `Compile(schema)`).
///
/// UTF-16 data with unpaired surrogates uses [`utf16::SchemaValidator`] directly.
pub struct SchemaValidator<'a> {
    root: strict::Value,
    _schema: std::marker::PhantomData<&'a Value>,
}

impl<'a> SchemaValidator<'a> {
    pub fn new(root: &'a Value) -> Self {
        Self {
            root: to_strict(root),
            _schema: std::marker::PhantomData,
        }
    }

    /// `Check(value)`.
    pub fn check(&self, value: &Value) -> bool {
        utf16::SchemaValidator::new(&self.root).check(&to_strict(value))
    }

    /// `Errors(value)`.
    pub fn errors(&self, value: &Value) -> Vec<ValidationError> {
        utf16::SchemaValidator::new(&self.root)
            .errors(&to_strict(value))
            .into_iter()
            .map(Into::into)
            .collect()
    }
}

/// Validate `value` against `schema` (TypeBox `Compile(schema).Check(value)`).
pub fn check_schema(schema: &Value, value: &Value) -> bool {
    utf16::check_schema(&to_strict(schema), &to_strict(value))
}

/// `Value.Convert(schema, value)` of TypeBox 1.3.27 for a schema built with TypeBox.
pub fn typebox_convert(schema: &Value, value: Value) -> Value {
    to_serde(utf16::typebox_convert(
        &to_strict(schema),
        strict::Value::from(value),
    ))
}

/// Coerce a value toward a plain JSON schema using AJV-compatible primitive rules.
pub fn coerce_with_json_schema(value: Value, schema: &Value) -> Value {
    to_serde(utf16::coerce_with_json_schema(
        strict::Value::from(value),
        &to_strict(schema),
    ))
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct ToolValidationError(pub String);

/// Finds a tool by name and validates the tool call arguments against its schema.
pub fn validate_tool_call(
    tools: &[Tool],
    tool_call: &ToolCall,
) -> Result<Value, ToolValidationError> {
    let tool = tools
        .iter()
        .find(|t| t.name == tool_call.name)
        .ok_or_else(|| ToolValidationError(format!("Tool \"{}\" not found", tool_call.name)))?;
    validate_tool_arguments(tool, tool_call)
}

/// Validates tool call arguments against the tool's schema, returning the coerced arguments.
pub fn validate_tool_arguments(
    tool: &Tool,
    tool_call: &ToolCall,
) -> Result<Value, ToolValidationError> {
    utf16::validate_tool_arguments(
        &tool_call.name.as_str().into(),
        &to_strict(&tool.parameters),
        tool.typebox,
        &strict::Value::from(Value::Object(tool_call.arguments.clone())),
        tool_call
            .arguments_error
            .as_deref()
            .map(strict::JsonString::from)
            .as_ref(),
    )
    .map(to_serde)
    .map_err(|message| ToolValidationError(message.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, json};

    fn plain(schema: Value, value: Value) -> (Tool, ToolCall) {
        let tool = Tool::new(
            "echo",
            "Echo tool",
            json!({"type": "object", "properties": {"value": schema}, "required": ["value"]}),
        );
        let mut arguments = Map::new();
        arguments.insert("value".into(), value);
        let call = ToolCall {
            id: "tool-1".into(),
            name: "echo".into(),
            arguments,
            ..Default::default()
        };
        (tool, call)
    }

    #[test]
    fn coerces_string_to_number_for_typebox_like_schema() {
        let tool = Tool::new(
            "echo",
            "Echo tool",
            json!({"type": "object", "properties": {"count": {"type": "number"}}, "required": ["count"]}),
        );
        let mut arguments = Map::new();
        arguments.insert("count".into(), json!("42"));
        let call = ToolCall {
            id: "t".into(),
            name: "echo".into(),
            arguments,
            ..Default::default()
        };
        assert_eq!(
            validate_tool_arguments(&tool, &call).unwrap(),
            json!({"count": 42})
        );
    }

    #[test]
    fn coerces_with_ajv_compatible_rules() {
        let cases = vec![
            (json!({"type": "number"}), json!("42"), json!(42)),
            (json!({"type": "number"}), json!(true), json!(1)),
            (json!({"type": "number"}), json!(null), json!(0)),
            (json!({"type": "integer"}), json!("42"), json!(42)),
            (json!({"type": "boolean"}), json!("true"), json!(true)),
            (json!({"type": "boolean"}), json!("false"), json!(false)),
            (json!({"type": "boolean"}), json!(1), json!(true)),
            (json!({"type": "boolean"}), json!(0), json!(false)),
            (json!({"type": "string"}), json!(null), json!("")),
            (json!({"type": "string"}), json!(true), json!("true")),
            (json!({"type": "null"}), json!(""), json!(null)),
            (json!({"type": "null"}), json!(0), json!(null)),
            (json!({"type": "null"}), json!(false), json!(null)),
            (
                json!({"type": ["number", "string"]}),
                json!("1"),
                json!("1"),
            ),
            (json!({"type": ["boolean", "number"]}), json!("1"), json!(1)),
        ];
        for (schema, input, expected) in cases {
            let (tool, call) = plain(schema.clone(), input.clone());
            assert_eq!(
                validate_tool_arguments(&tool, &call).unwrap(),
                json!({"value": expected}),
                "{schema} {input}"
            );
        }
    }

    #[test]
    fn treats_null_as_omission_for_optional_non_nullable_properties() {
        let tool = Tool::new(
            "echo",
            "Echo tool",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "number"},
                    "nullable": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                    "metadata": {"type": "object", "properties": {"enabled": {"type": "boolean"}}}
                },
                "required": ["path", "metadata"]
            }),
        );
        let arguments = json!({"path": "file.txt", "offset": null, "nullable": null, "metadata": {"enabled": null}});
        let call = ToolCall {
            id: "t".into(),
            name: "echo".into(),
            arguments: arguments.as_object().unwrap().clone(),
            ..Default::default()
        };
        assert_eq!(
            validate_tool_arguments(&tool, &call).unwrap(),
            json!({"path": "file.txt", "nullable": null, "metadata": {}})
        );
    }

    #[test]
    fn preserves_optional_nulls_whose_referenced_schema_is_nullable() {
        let tool = Tool::new(
            "echo",
            "Echo tool",
            json!({
                "type": "object",
                "properties": {"value": {"$ref": "#/$defs/value"}},
                "$defs": {"value": {"anyOf": [{"type": "number"}, {"type": "null"}]}}
            }),
        );
        let call = ToolCall {
            id: "t".into(),
            name: "echo".into(),
            arguments: json!({"value": null}).as_object().unwrap().clone(),
            ..Default::default()
        };
        assert_eq!(
            validate_tool_arguments(&tool, &call).unwrap(),
            json!({"value": null})
        );
    }

    #[test]
    fn nullable_unions() {
        let (tool, call) = plain(
            json!({"anyOf": [{"type": "number"}, {"type": "null"}]}),
            json!(null),
        );
        assert_eq!(
            validate_tool_arguments(&tool, &call).unwrap(),
            json!({"value": null})
        );
        let (tool, call) = plain(
            json!({"oneOf": [{"type": "number"}, {"type": "null"}]}),
            json!(null),
        );
        assert_eq!(
            validate_tool_arguments(&tool, &call).unwrap(),
            json!({"value": null})
        );
        let (tool, call) = plain(
            json!({"anyOf": [{"type": "number"}, {"type": "null"}]}),
            json!("42"),
        );
        assert_eq!(
            validate_tool_arguments(&tool, &call).unwrap(),
            json!({"value": 42})
        );
        let (tool, call) = plain(
            json!({"type": ["array", "null"], "items": {"type": "string"}}),
            json!(null),
        );
        assert_eq!(
            validate_tool_arguments(&tool, &call).unwrap(),
            json!({"value": null})
        );
    }

    #[test]
    fn rejects_invalid_coercions() {
        let cases = vec![
            (json!({"type": "boolean"}), json!("1")),
            (json!({"type": "boolean"}), json!("0")),
            (json!({"type": "null"}), json!("null")),
            (json!({"type": "integer"}), json!("42.1")),
        ];
        for (schema, input) in cases {
            let (tool, call) = plain(schema, input);
            let err = validate_tool_arguments(&tool, &call).unwrap_err();
            assert!(err.0.contains("Validation failed"), "{}", err.0);
        }
    }

    #[test]
    fn reports_malformed_arguments() {
        let tool = Tool::new(
            "echo",
            "Echo tool",
            json!({"type": "object", "properties": {"value": {"type": "string"}}}),
        );
        let call = ToolCall {
            id: "t".into(),
            name: "echo".into(),
            arguments_error: Some("Unexpected token".into()),
            ..Default::default()
        };
        assert_eq!(
            validate_tool_arguments(&tool, &call).unwrap_err().0,
            "Invalid arguments for tool \"echo\": Unexpected token"
        );
    }

    #[test]
    fn formats_errors_like_typebox() {
        let tool = Tool::new(
            "read",
            "Read",
            json!({"type": "object", "properties": {"path": {"type": "string"}, "limit": {"type": "number", "minimum": 1}}, "required": ["path"]}),
        );
        let call = ToolCall {
            id: "t".into(),
            name: "read".into(),
            arguments: json!({"limit": 0}).as_object().unwrap().clone(),
            ..Default::default()
        };
        let err = validate_tool_arguments(&tool, &call).unwrap_err().0;
        assert!(err.starts_with("Validation failed for tool \"read\":\n  - path: must have required properties path\n  - limit: must be >= 1"), "{err}");
        assert!(err.ends_with("Received arguments:\n{\n  \"limit\": 0\n}"));
    }

    // Expected errors are the output of TypeBox 1.3.27 `Compile(schema).Errors(value)`.
    #[test]
    fn reports_additional_properties_like_typebox() {
        let schema = json!({
            "type": "object",
            "properties": { "name": { "type": "string" }, "nested": { "type": "object", "properties": {}, "additionalProperties": false } },
            "additionalProperties": false
        });
        let errors: Vec<(String, String, String, String)> = SchemaValidator::new(&schema)
            .errors(&json!({ "name": "x", "unsupported": true, "nested": { "a": 1 } }))
            .into_iter()
            .map(|e| (e.keyword, e.schema_path, e.instance_path, e.message))
            .collect();
        let expected = [
            (
                "boolean",
                "#/additionalProperties",
                "/unsupported",
                "schema is false",
            ),
            (
                "additionalProperties",
                "#",
                "",
                "must not have additional properties",
            ),
            (
                "boolean",
                "#/properties/nested/additionalProperties",
                "/nested/a",
                "schema is false",
            ),
            (
                "additionalProperties",
                "#/properties/nested",
                "/nested",
                "must not have additional properties",
            ),
        ];
        assert_eq!(
            errors,
            expected
                .iter()
                .map(|(a, b, c, d)| (a.to_string(), b.to_string(), c.to_string(), d.to_string()))
                .collect::<Vec<_>>()
        );

        let schema = json!({ "type": "object", "properties": { "name": { "type": "string" } }, "additionalProperties": { "type": "number" } });
        let errors: Vec<(String, String, String)> = SchemaValidator::new(&schema)
            .errors(&json!({ "name": "x", "extra": "s" }))
            .into_iter()
            .map(|e| (e.keyword, e.schema_path, e.instance_path))
            .collect();
        assert_eq!(
            errors,
            vec![
                (
                    "type".to_string(),
                    "#/additionalProperties".to_string(),
                    "/extra".to_string()
                ),
                (
                    "additionalProperties".to_string(),
                    "#".to_string(),
                    String::new()
                ),
            ]
        );
    }

    // Expected values are pinned pi-ai `validateToolArguments` and TypeBox 1.3.27 `Compile`
    // outputs for JavaScript strings with unpaired surrogates, run under Node.
    #[test]
    fn utf16_engine_keeps_unpaired_surrogates() {
        let parse = |text: &str| strict::parse(text).unwrap();
        let name = parse(r#""t\ud800""#).as_string().unwrap().clone();
        let cases = [
            (
                r#"{"type":"object","properties":{"text":{"type":"string"},"n":{"type":"number"}}}"#,
                r#"{"text":"a\udc00b","n":"5","k\ud800":"v\udfff"}"#,
                Ok(r#"{"text":"a\udc00b","n":5,"k\ud800":"v\udfff"}"#),
            ),
            (
                r#"{"type":"object","properties":{"s":{"type":"string","enum":["\ud800"]}},"required":["s"]}"#,
                r#"{"s":"\udc00"}"#,
                Err(
                    r#""Validation failed for tool \"t\ud800\":\n  - s: must be equal to one of the allowed values\n\nReceived arguments:\n{\n  \"s\": \"\\udc00\"\n}""#,
                ),
            ),
        ];
        for (schema, arguments, expected) in cases {
            let actual = utf16::validate_tool_arguments(
                &name,
                &parse(schema),
                false,
                &parse(arguments),
                None,
            );
            match expected {
                Ok(value) => assert_eq!(actual.unwrap().to_string(), value),
                Err(message) => {
                    assert_eq!(&actual.unwrap_err(), parse(message).as_string().unwrap())
                }
            }
        }
        let schema = parse(
            r#"{"type":"object","properties":{"s":{"type":"string","maxLength":1}},"additionalProperties":false}"#,
        );
        let validator = utf16::SchemaValidator::new(&schema);
        assert!(validator.check(&parse(r#"{"s":"\ud800"}"#)));
        let errors: Vec<(String, String)> = validator
            .errors(&parse(r#"{"k\udc00":1}"#))
            .into_iter()
            .map(|e| {
                (
                    strict::stringify(&strict::Value::String(e.instance_path)),
                    e.message.to_string_lossy(),
                )
            })
            .collect();
        assert_eq!(
            errors,
            [
                (r#""/k\udc00""#.to_string(), "schema is false".to_string()),
                (
                    r#""""#.to_string(),
                    "must not have additional properties".to_string()
                ),
            ]
        );
    }

    // TypeBox 1.3.27 `Compile({ type: "string", pattern }).Check(value)` under Node (`u` flag).
    #[test]
    fn utf16_patterns_follow_javascript_unicode_regexes() {
        let cases = [
            (r#""^.$""#, r#""\ud800""#, true),
            (r#""^[\\ud800-\\udbff]$""#, r#""\ud800""#, true),
            (r#""^[\\ud800-\\udbff]$""#, r#""\ud83d\ude00""#, false),
            (r#""\\ud83d""#, r#""\ud83d\ude00""#, false),
            (r#""^\\ud83d\\ude00$""#, r#""\ud83d\ude00""#, true),
            (r#""^\\udc00$""#, r#""\udc00""#, true),
            (r#""^[^a]$""#, r#""\udfff""#, true),
            (r#""^\\S\\S$""#, r#""\ud800\udc00""#, false),
            (r#""^..$""#, r#""a\udc00""#, true),
            (r#""^\\u0041$""#, r#""A""#, true),
        ];
        for (pattern, value, expected) in cases {
            let schema =
                strict::parse(&format!(r#"{{"type":"string","pattern":{pattern}}}"#)).unwrap();
            let value = strict::parse(value).unwrap();
            assert_eq!(
                utf16::check_schema(&schema, &value),
                expected,
                "{pattern} {value}"
            );
        }
    }
}
