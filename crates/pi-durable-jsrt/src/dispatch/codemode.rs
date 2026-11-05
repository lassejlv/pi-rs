//! `@earendil-works/pi-codemode` (`js/pi-codemode/`, `codemode.sdk.*` over the native
//! `pi-codemode` crate). Durable code mode (`@earendil-works/pi-durable/codemode`) runs in the
//! hosted stock service and reaches it through `pi-durable:internal/transport`, not here.

use pi_codemode::declarations::json::{
    JsonToolDeclaration, render_json_declarations, render_json_tool_output_type,
    render_json_tool_sample, render_json_tool_signature, schema_to_json_type,
};
use pi_json::strict::{JsonString, Value};

use crate::errors::{HostError, HostResult};
use crate::host::{Args, Call, Dispatch, Methods};
use crate::value::HostValue;

pub mod live;
pub mod sdk;

/// The code-mode method group.
pub struct CodemodeDispatch;

fn strict(value: &HostValue) -> HostResult<Value> {
    value.to_strict_omitting_undefined()
}

/// An optional character budget (`undefined` keeps the renderer's default).
fn budget(value: &HostValue) -> HostResult<Option<usize>> {
    match value {
        HostValue::Undefined | HostValue::Null => Ok(None),
        HostValue::Number(n) if n.is_finite() && *n >= 0.0 => Ok(Some(*n as usize)),
        HostValue::Number(n) if *n == f64::INFINITY => Ok(Some(usize::MAX)),
        _ => Err(HostError::type_error(
            "maxChars must be a non-negative number",
        )),
    }
}

fn optional_string(value: Option<&HostValue>) -> HostResult<Option<JsonString>> {
    match value {
        None | Some(HostValue::Undefined) => Ok(None),
        Some(HostValue::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(HostError::type_error("expected a string")),
    }
}

fn optional_schema(value: Option<&HostValue>) -> HostResult<Option<Value>> {
    match value {
        None | Some(HostValue::Undefined) => Ok(None),
        Some(v) => strict(v).map(Some),
    }
}

/// A `CodemodeTool`'s declaration fields (`execute` and other members are ignored).
fn declaration(value: &HostValue) -> HostResult<JsonToolDeclaration> {
    Ok(JsonToolDeclaration {
        name: match value.get("name") {
            Some(HostValue::String(s)) => s.clone(),
            _ => return Err(HostError::type_error("tool name must be a string")),
        },
        description: optional_string(value.get("description"))?,
        input_schema: optional_schema(value.get("inputSchema"))?,
        output_schema: optional_schema(value.get("outputSchema"))?,
        signature: optional_string(value.get("signature"))?,
    })
}

fn declarations(value: Option<&HostValue>) -> HostResult<Vec<JsonToolDeclaration>> {
    match value {
        None | Some(HostValue::Undefined) => Ok(Vec::new()),
        Some(HostValue::Array(items)) => items.iter().map(declaration).collect(),
        Some(_) => Err(HostError::type_error("expected an array of tools")),
    }
}

fn string(s: JsonString) -> HostValue {
    HostValue::String(s)
}

fn sdk_source_parse(_call: &Call, args: Args) -> HostResult<HostValue> {
    let input = args.string(0)?;
    match pi_codemode::parse_codemode_source_utf16(input.units()) {
        Ok(parsed) => {
            let mut options = Vec::new();
            if let Some(n) = parsed.options.max_output_tokens {
                options.push(("maxOutputTokens", HostValue::from(n as f64)));
            }
            if let Some(n) = parsed.options.timeout_ms {
                options.push(("timeoutMs", HostValue::from(n as f64)));
            }
            Ok(HostValue::object([
                ("code", string(parsed.code)),
                ("options", HostValue::object(options)),
            ]))
        }
        // The glue rethrows this as its `CodemodeSourceError`.
        Err(error) => Ok(HostValue::object([("error", string(error.message))])),
    }
}

impl Dispatch for CodemodeDispatch {
    fn register(&self, m: &mut Methods) {
        m.sync("codemode.sdk.constants", |_call, _args| {
            Ok(HostValue::object([
                (
                    "DEFAULT_INPUT_SCHEMA_MAX_CHARS",
                    HostValue::from(pi_codemode::DEFAULT_INPUT_SCHEMA_MAX_CHARS as f64),
                ),
                (
                    "MAX_OUTPUT_CHARS",
                    HostValue::from(pi_codemode::MAX_OUTPUT_CHARS as f64),
                ),
                (
                    "MAX_OUTPUT_ITEMS",
                    HostValue::from(pi_codemode::MAX_OUTPUT_ITEMS as f64),
                ),
                (
                    "MAX_STORE_TOTAL_CHARS",
                    HostValue::from(pi_codemode::MAX_STORE_TOTAL_CHARS as f64),
                ),
                (
                    "MAX_STORE_VALUE_CHARS",
                    HostValue::from(pi_codemode::MAX_STORE_VALUE_CHARS as f64),
                ),
                (
                    "DEFAULT_INTERRUPT_BUDGET",
                    HostValue::from(pi_codemode::DEFAULT_INTERRUPT_BUDGET as f64),
                ),
                (
                    "CODEMODE_OPTIONS_PREFIX",
                    HostValue::string(pi_codemode::CODEMODE_OPTIONS_PREFIX),
                ),
                (
                    "CODEMODE_SOURCE_GRAMMAR",
                    HostValue::string(pi_codemode::CODEMODE_SOURCE_GRAMMAR),
                ),
            ]))
        });
        m.sync("codemode.sdk.schemaToType", |_call, args| {
            let schema = strict(args.get(0))?;
            Ok(string(schema_to_json_type(&schema, budget(args.get(1))?)))
        });
        m.sync("codemode.sdk.renderToolSignature", |_call, args| {
            Ok(string(render_json_tool_signature(
                &declaration(args.get(0))?,
                budget(args.get(1))?,
            )))
        });
        m.sync("codemode.sdk.renderToolSample", |_call, args| {
            Ok(string(render_json_tool_sample(
                &declaration(args.get(0))?,
                budget(args.get(1))?,
            )))
        });
        m.sync("codemode.sdk.renderToolOutputType", |_call, args| {
            Ok(string(render_json_tool_output_type(
                optional_schema(Some(args.get(0)))?.as_ref(),
            )))
        });
        m.sync("codemode.sdk.renderDeclarations", |_call, args| {
            let options = args.get(0);
            Ok(string(render_json_declarations(
                &declarations(options.get("tools"))?,
                &declarations(options.get("globals"))?,
            )))
        });
        m.sync("codemode.sdk.identifier", |_call, args| {
            Ok(HostValue::string(
                &pi_codemode::declarations::json::to_json_codemode_identifier(args.string(0)?),
            ))
        });
        m.sync("codemode.sdk.parseSource", sdk_source_parse);
        m.sync("codemode.sdk.prelude", |_call, _args| {
            Ok(HostValue::string(&pi_codemode::PRELUDE_SOURCE))
        });
        sdk::register(m);
    }
}
