use super::{
    CODEMODE_OPTIONS_PREFIX, CodemodeSourceOptions, MAX_SAFE_INTEGER, MAX_TIMEOUT_MS,
    SUPPORTED_FIELDS, SUPPORTED_FIELDS_TEXT,
};
use pi_json::strict::{self, JsonString, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCodemodeSourceUtf16 {
    pub code: JsonString,
    pub options: CodemodeSourceOptions,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct CodemodeSourceErrorUtf16 {
    pub message: JsonString,
}
impl CodemodeSourceErrorUtf16 {
    pub fn name(&self) -> &'static str {
        "CodemodeSourceError"
    }
    fn new(message: impl Into<JsonString>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
fn whitespace(unit: &u16) -> bool {
    char::from_u32(u32::from(*unit)).is_some_and(crate::js::is_js_whitespace)
}
fn trim_start(units: &[u16]) -> &[u16] {
    &units[units.iter().take_while(|unit| whitespace(unit)).count()..]
}
fn trim(units: &[u16]) -> &[u16] {
    let units = trim_start(units);
    &units[..units.len()
        - units
            .iter()
            .rev()
            .take_while(|unit| whitespace(unit))
            .count()]
}
fn safe_integer(value: &Value) -> Option<u64> {
    let number = value.as_f64()?;
    (number.fract() == 0.0 && (0.0..=MAX_SAFE_INTEGER).contains(&number)).then_some(number as u64)
}
fn parse_options(units: &[u16]) -> Result<CodemodeSourceOptions, CodemodeSourceErrorUtf16> {
    if units.is_empty() {
        return Err(CodemodeSourceErrorUtf16::new(format!(
            "@options must be a JSON object with supported fields {SUPPORTED_FIELDS_TEXT}"
        )));
    }
    // JSON permits raw surrogate units inside strings. Escape only those units
    // for the UTF-8 parser; values and property names remain lossless.
    let mut json = String::new();
    for scalar in char::decode_utf16(units.iter().copied()) {
        match scalar {
            Ok(ch) => json.push(ch),
            Err(error) => json.push_str(&format!("\\u{:04x}", error.unpaired_surrogate())),
        }
    }
    let value = strict::parse(&json).map_err(|error| {
        CodemodeSourceErrorUtf16::new(format!(
            "@options must be valid JSON with supported fields {SUPPORTED_FIELDS_TEXT}: {error}"
        ))
    })?;
    let Value::Object(fields) = value else {
        return Err(CodemodeSourceErrorUtf16::new(format!(
            "@options must be a JSON object with supported fields {SUPPORTED_FIELDS_TEXT}"
        )));
    };
    for (key, _) in strict::object_entries(&fields) {
        if !SUPPORTED_FIELDS
            .iter()
            .any(|field| key.units().iter().copied().eq(field.encode_utf16()))
        {
            let message = format!("@options only supports {SUPPORTED_FIELDS_TEXT}; got `")
                .encode_utf16()
                .chain(key.units().iter().copied())
                .chain("`".encode_utf16())
                .collect::<Vec<_>>();
            return Err(CodemodeSourceErrorUtf16::new(JsonString::from_utf16(
                message,
            )));
        }
    }
    let mut options = CodemodeSourceOptions::default();
    if let Some(value) = fields.get(&JsonString::from("max_output_tokens")) {
        options.max_output_tokens = Some(safe_integer(value).ok_or_else(|| {
            CodemodeSourceErrorUtf16::new(
                "@options field `max_output_tokens` must be a non-negative safe integer",
            )
        })?);
    }
    if let Some(value) = fields.get(&JsonString::from("timeout_ms")) {
        options.timeout_ms = Some(safe_integer(value).filter(|n| *n != 0 && *n <= MAX_TIMEOUT_MS).ok_or_else(|| {
            CodemodeSourceErrorUtf16::new(format!("@options field `timeout_ms` must be a positive integer up to {MAX_TIMEOUT_MS}"))
        })?);
    }
    Ok(options)
}
/// Parse an options line without converting JavaScript source or errors to UTF-8.
pub fn parse_codemode_source_utf16(
    input: &[u16],
) -> Result<ParsedCodemodeSourceUtf16, CodemodeSourceErrorUtf16> {
    if trim(input).is_empty() {
        return Err(CodemodeSourceErrorUtf16::new(
            "Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `// @options: {\"max_output_tokens\": 1000}`.",
        ));
    }
    let prefix: Vec<u16> = CODEMODE_OPTIONS_PREFIX.encode_utf16().collect();
    let lines: Vec<&[u16]> = input.split(|unit| *unit == 10).collect();
    if let Some((line, _)) = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| trim_start(line).starts_with(&prefix))
    {
        return Err(CodemodeSourceErrorUtf16::new(format!(
            "The {CODEMODE_OPTIONS_PREFIX} line must be the first line of the input; found one on line {}",
            line + 1
        )));
    }
    let first = lines[0].strip_suffix(&[13]).unwrap_or(lines[0]);
    let first = trim_start(first);
    if !first.starts_with(&prefix) {
        return Ok(ParsedCodemodeSourceUtf16 {
            code: JsonString::from_utf16(input.to_vec()),
            options: Default::default(),
        });
    }
    let code = input
        .iter()
        .position(|unit| *unit == 10)
        .map(|newline| &input[newline..])
        .unwrap_or_default();
    if trim(code).is_empty() {
        return Err(CodemodeSourceErrorUtf16::new(
            "The @options line must be followed by JavaScript source on subsequent lines",
        ));
    }
    Ok(ParsedCodemodeSourceUtf16 {
        code: JsonString::from_utf16(code.to_vec()),
        options: parse_options(trim(&first[prefix.len()..]))?,
    })
}
