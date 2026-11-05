//! Codemode source format: JavaScript, optionally preceded by one options line.
//!
//! ```js
//! // @options: {"max_output_tokens": 2000, "timeout_ms": 30000}
//! const text = await tools.read({ path: "package.json" });
//! text(JSON.parse(text).name);
//! ```

use serde_json::Value;

use crate::js;
mod utf16;
pub use utf16::{CodemodeSourceErrorUtf16, ParsedCodemodeSourceUtf16, parse_codemode_source_utf16};

/// Prefix of the options line.
pub const CODEMODE_OPTIONS_PREFIX: &str = "// @options:";

const SUPPORTED_FIELDS: [&str; 2] = ["max_output_tokens", "timeout_ms"];
const SUPPORTED_FIELDS_TEXT: &str = "`max_output_tokens` and `timeout_ms`";
/// Largest delay `setTimeout` supports, which bounds `timeout_ms`.
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Lark grammar for providers with grammar-constrained tool input. It only fixes the shape of the
/// options line; the options JSON and the code are checked by [`parse_codemode_source`].
pub const CODEMODE_SOURCE_GRAMMAR: &str = r#"
start: options_source | plain_source
options_source: OPTIONS_LINE NEWLINE SOURCE
plain_source: SOURCE

OPTIONS_LINE: /[ \t]*\/\/ @options:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;

/// Options of a script's first line.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeSourceOptions {
    /// Token budget for the script's output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Hard deadline for the whole script in milliseconds, including tool calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// A script split into its code and options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCodemodeSource {
    /// The script with the options line replaced by an empty line, so line numbers are unchanged.
    pub code: String,
    pub options: CodemodeSourceOptions,
}

/// Invalid script source (`CodemodeSourceError` in TS).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct CodemodeSourceError {
    pub message: String,
}

impl CodemodeSourceError {
    fn new(message: impl Into<String>) -> Self {
        CodemodeSourceError { message: message.into() }
    }

    /// `error.name` in TS.
    pub fn name(&self) -> &'static str {
        "CodemodeSourceError"
    }
}

/// A non-negative safe integer (`Number.isSafeInteger(value) && value >= 0`).
fn safe_integer(value: &Value) -> Option<u64> {
    let number = value.as_f64()?;
    (number.fract() == 0.0 && (0.0..=MAX_SAFE_INTEGER).contains(&number)).then_some(number as u64)
}

fn parse_options(directive: &str) -> Result<CodemodeSourceOptions, CodemodeSourceError> {
    if directive.is_empty() {
        return Err(CodemodeSourceError::new(format!(
            "@options must be a JSON object with supported fields {SUPPORTED_FIELDS_TEXT}"
        )));
    }
    let value: Value = serde_json::from_str(directive).map_err(|error| {
        CodemodeSourceError::new(format!(
            "@options must be valid JSON with supported fields {SUPPORTED_FIELDS_TEXT}: {error}"
        ))
    })?;
    let Value::Object(fields) = value else {
        return Err(CodemodeSourceError::new(format!(
            "@options must be a JSON object with supported fields {SUPPORTED_FIELDS_TEXT}"
        )));
    };
    for key in js::object_keys(&fields) {
        if !SUPPORTED_FIELDS.contains(&key.as_str()) {
            return Err(CodemodeSourceError::new(format!(
                "@options only supports {SUPPORTED_FIELDS_TEXT}; got `{key}`"
            )));
        }
    }
    let mut options = CodemodeSourceOptions::default();
    if let Some(value) = fields.get("max_output_tokens") {
        let Some(tokens) = safe_integer(value) else {
            return Err(CodemodeSourceError::new(
                "@options field `max_output_tokens` must be a non-negative safe integer",
            ));
        };
        options.max_output_tokens = Some(tokens);
    }
    if let Some(value) = fields.get("timeout_ms") {
        match safe_integer(value) {
            Some(timeout) if timeout != 0 && timeout <= MAX_TIMEOUT_MS => options.timeout_ms = Some(timeout),
            _ => {
                return Err(CodemodeSourceError::new(format!(
                    "@options field `timeout_ms` must be a positive integer up to {MAX_TIMEOUT_MS}"
                )));
            }
        }
    }
    Ok(options)
}

/// Split an optional first-line `// @options: {...}` from the script. Fails with
/// [`CodemodeSourceError`] for empty input, invalid options, and an options line that is not the
/// first line, which would otherwise be ignored without notice.
pub fn parse_codemode_source(input: &str) -> Result<ParsedCodemodeSource, CodemodeSourceError> {
    if js::trim(input).is_empty() {
        return Err(CodemodeSourceError::new(
            "Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `// @options: {\"max_output_tokens\": 1000}`.",
        ));
    }
    let lines: Vec<&str> = input.split('\n').collect();
    if let Some(misplaced) =
        lines.iter().enumerate().skip(1).position(|(_, line)| js::trim_start(line).starts_with(CODEMODE_OPTIONS_PREFIX))
    {
        return Err(CodemodeSourceError::new(format!(
            "The {CODEMODE_OPTIONS_PREFIX} line must be the first line of the input; found one on line {}",
            misplaced + 2
        )));
    }
    let first_line = lines[0].strip_suffix('\r').unwrap_or(lines[0]);
    let trimmed = js::trim_start(first_line);
    if !trimmed.starts_with(CODEMODE_OPTIONS_PREFIX) {
        return Ok(ParsedCodemodeSource { code: input.to_string(), options: CodemodeSourceOptions::default() });
    }
    let code = match input.find('\n') {
        Some(newline) => input[newline..].to_string(),
        None => String::new(),
    };
    if js::trim(&code).is_empty() {
        return Err(CodemodeSourceError::new(
            "The @options line must be followed by JavaScript source on subsequent lines",
        ));
    }
    let options = parse_options(js::trim(&trimmed[CODEMODE_OPTIONS_PREFIX.len()..]))?;
    Ok(ParsedCodemodeSource { code, options })
}
