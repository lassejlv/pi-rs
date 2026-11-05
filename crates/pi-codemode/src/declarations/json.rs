//! Lossless declaration rendering over JavaScript UTF-16 strings and strict JSON.
use super::DEFAULT_INPUT_SCHEMA_MAX_CHARS;
use pi_json::strict::{self, JsonString, Map, Value};
use std::collections::HashSet;

const MAX_REF_EXPANSIONS: usize = 32;
const MAX_TAG_CHARS: usize = 80;
const TAG_KEYWORDS: [&str; 11] = [
    "minimum",
    "exclusiveMinimum",
    "maximum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minItems",
    "maxItems",
];
const LOWER_BOUNDS: [&str; 4] = ["minimum", "exclusiveMinimum", "minLength", "minItems"];
const UPPER_BOUNDS: [&str; 4] = ["maximum", "exclusiveMaximum", "maxLength", "maxItems"];

#[derive(Clone, Debug, Default)]
/// The declaration fields used by rendering, without executable callbacks.
pub struct JsonToolDeclaration {
    pub name: JsonString,
    pub description: Option<JsonString>,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    pub signature: Option<JsonString>,
}
trait Text {
    fn extend_into(&self, out: &mut Vec<u16>);
}
impl Text for str {
    fn extend_into(&self, out: &mut Vec<u16>) {
        out.extend(self.encode_utf16());
    }
}
impl Text for String {
    fn extend_into(&self, out: &mut Vec<u16>) {
        self.as_str().extend_into(out);
    }
}
impl Text for JsonString {
    fn extend_into(&self, out: &mut Vec<u16>) {
        out.extend_from_slice(self.units());
    }
}
impl<T: Text + ?Sized> Text for &T {
    fn extend_into(&self, out: &mut Vec<u16>) {
        (*self).extend_into(out);
    }
}
macro_rules! text { ($($part:expr),* $(,)?) => {{ let mut out = Vec::new(); $($part.extend_into(&mut out);)* JsonString::from_utf16(out) }}; }
fn join(items: impl IntoIterator<Item = JsonString>, separator: &str) -> JsonString {
    let mut out = JsonString::default();
    for (i, item) in items.into_iter().enumerate() {
        if i != 0 {
            out.append(&separator.into());
        }
        out.append(&item);
    }
    out
}
fn get<'a>(object: &'a Map, key: &str) -> Option<&'a Value> {
    object.get(&JsonString::from(key))
}
fn has(object: &Map, key: &str) -> bool {
    get(object, key).is_some()
}
fn trim(text: &JsonString) -> JsonString {
    let space = |u: &u16| matches!(*u,0x09..=0x0d|0x20|0xa0|0x1680|0x2000..=0x200a|0x2028|0x2029|0x202f|0x205f|0x3000|0xfeff);
    let units = text.units();
    let first = units.iter().position(|u| !space(u)).unwrap_or(units.len());
    let last = units
        .iter()
        .rposition(|u| !space(u))
        .map_or(first, |i| i + 1);
    JsonString::from_utf16(units[first..last].to_vec())
}
fn replace(text: &JsonString, from: &str, to: &str) -> JsonString {
    let from: Vec<u16> = from.encode_utf16().collect();
    let to: Vec<u16> = to.encode_utf16().collect();
    let units = text.units();
    let mut out = Vec::new();
    let mut at = 0;
    while at < units.len() {
        if units[at..].starts_with(&from) {
            out.extend_from_slice(&to);
            at += from.len();
        } else {
            out.push(units[at]);
            at += 1;
        }
    }
    JsonString::from_utf16(out)
}
fn contains(text: &JsonString, needle: &str) -> bool {
    let needle: Vec<u16> = needle.encode_utf16().collect();
    text.units()
        .windows(needle.len())
        .any(|part| part == needle)
}
fn line_terminator(u: &u16) -> bool {
    matches!(*u, 0x0a | 0x0d | 0x2028 | 0x2029)
}

/// Source identifiers replace each non-ASCII code point with one underscore.
pub fn to_json_codemode_identifier(name: &JsonString) -> String {
    let mut out = String::new();
    for character in char::decode_utf16(name.units().iter().copied()) {
        let valid = character.ok().filter(|c| {
            c.is_ascii_alphabetic()
                || *c == '_'
                || *c == '$'
                || (!out.is_empty() && c.is_ascii_digit())
        });
        out.push(valid.unwrap_or('_'));
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}
/// Render script declarations without converting names, comments or schemas to UTF-8.
pub fn render_json_declarations(
    tools: &[JsonToolDeclaration],
    globals: &[JsonToolDeclaration],
) -> JsonString {
    let mut sections = Vec::new();
    if !tools.is_empty() {
        sections.push(text!(
            "declare const tools: {\n",
            join(
                tools.iter().map(|tool| text!(
                    doc_comment(tool.description.as_ref(), "  "),
                    "  ",
                    render_json_tool_signature(tool, None)
                )),
                "\n"
            ),
            "\n};"
        ));
    }
    let mut namespaces: Vec<(JsonString, Vec<JsonString>)> = Vec::new();
    for global in globals {
        let units = global.name.units();
        if let Some(dot) = units.iter().position(|u| *u == 46) {
            let namespace = JsonString::from_utf16(units[..dot].to_vec());
            let member = render_global(
                &JsonString::from_utf16(units[dot + 1..].to_vec()),
                global,
                "  ",
            );
            match namespaces.iter_mut().find(|(name, _)| name == &namespace) {
                Some((_, members)) => members.push(member),
                None => namespaces.push((namespace, vec![member])),
            }
        } else {
            sections.push(render_global(
                &text!("declare function ", global.name),
                global,
                "",
            ));
        }
    }
    for (name, members) in namespaces {
        sections.push(text!(
            "declare const ",
            name,
            ": {\n",
            join(members, "\n"),
            "\n};"
        ));
    }
    join(sections, "\n\n")
}
/// Render a member signature, bounding the input by its UTF-16 length.
pub fn render_json_tool_signature(
    tool: &JsonToolDeclaration,
    max_chars: Option<usize>,
) -> JsonString {
    let input = tool.input_schema.as_ref().map_or_else(
        || "unknown".into(),
        |s| schema_to_json_type(s, Some(max_chars.unwrap_or(DEFAULT_INPUT_SCHEMA_MAX_CHARS))),
    );
    text!(
        to_json_codemode_identifier(&tool.name),
        "(args: ",
        input,
        "): Promise<",
        render_json_tool_output_type(tool.output_schema.as_ref()),
        ">;"
    )
}
/// Render a trimmed description followed by its fenced declaration.
pub fn render_json_tool_sample(tool: &JsonToolDeclaration, max_chars: Option<usize>) -> JsonString {
    text!(
        tool.description.as_ref().map(trim).unwrap_or_default(),
        "\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { ",
        render_json_tool_signature(tool, max_chars),
        " };\n```"
    )
}
/// Render an output schema, or `unknown` when it is absent.
pub fn render_json_tool_output_type(schema: Option<&Value>) -> JsonString {
    schema.map_or_else(|| "unknown".into(), |s| schema_to_json_type(s, None))
}
fn render_global(head: &JsonString, tool: &JsonToolDeclaration, indent: &str) -> JsonString {
    let doc = doc_comment(tool.description.as_ref(), indent);
    if let Some(signature) = &tool.signature {
        return text!(doc, indent, head, signature, ";");
    }
    text!(
        doc,
        indent,
        head,
        "(args: ",
        render_json_tool_output_type(tool.input_schema.as_ref()),
        "): Promise<",
        render_json_tool_output_type(tool.output_schema.as_ref()),
        ">;"
    )
}
fn doc_comment(description: Option<&JsonString>, indent: &str) -> JsonString {
    let text = description.map(trim).unwrap_or_default();
    if text.units().is_empty() {
        return text;
    }
    let escaped = replace(&text, "*/", "*\\/");
    let lines: Vec<JsonString> = escaped
        .units()
        .split(|u| *u == 10)
        .map(|line| JsonString::from_utf16(line.strip_suffix(&[13]).unwrap_or(line).to_vec()))
        .collect();
    if lines.len() == 1 {
        return text!(indent, "/** ", lines[0], " */\n");
    }
    text!(
        indent,
        "/**\n",
        join(
            lines.iter().map(|line| if line.units().is_empty() {
                text!(indent, " *")
            } else {
                text!(indent, " * ", line)
            }),
            "\n"
        ),
        "\n",
        indent,
        " */\n"
    )
}
fn property_key(name: &JsonString) -> JsonString {
    if name
        .to_string_checked()
        .is_ok_and(|name| crate::identifier::is_identifier(&name))
    {
        name.clone()
    } else {
        strict::stringify(&Value::String(name.clone())).into()
    }
}
fn union(types: impl IntoIterator<Item = JsonString>) -> JsonString {
    let mut unique = Vec::new();
    for item in types {
        if !unique.contains(&item) {
            unique.push(item);
        }
    }
    if unique.iter().any(|s: &JsonString| s.equals_str("unknown")) {
        "unknown".into()
    } else if unique.is_empty() {
        "never".into()
    } else {
        join(unique, " | ")
    }
}
/// Render a strict JSON schema, applying budgets to UTF-16 code units.
pub fn schema_to_json_type(schema: &Value, max_chars: Option<usize>) -> JsonString {
    let render = |tags| {
        to_type(
            schema,
            &mut SchemaContext {
                root: schema,
                resolving: Vec::new(),
                expansions: 0,
                tags,
            },
        )
    };
    let rendered = render(true);
    let Some(max_chars) = max_chars else {
        return rendered;
    };
    if rendered.units().len() <= max_chars {
        return rendered;
    }
    let untagged = render(false);
    if untagged.units().len() <= max_chars {
        untagged
    } else {
        "unknown".into()
    }
}
struct SchemaContext<'a> {
    root: &'a Value,
    resolving: Vec<JsonString>,
    expansions: usize,
    tags: bool,
}
fn decode_uri_component(segment: &[u16]) -> Option<JsonString> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < segment.len() {
        if segment[at] != 37 {
            out.push(segment[at]);
            at += 1;
            continue;
        }
        let mut bytes = Vec::new();
        while at < segment.len() && segment[at] == 37 {
            let high = char::from_u32(*segment.get(at + 1)? as u32)?.to_digit(16)?;
            let low = char::from_u32(*segment.get(at + 2)? as u32)?.to_digit(16)?;
            bytes.push((high * 16 + low) as u8);
            at += 3;
        }
        out.extend(String::from_utf8(bytes).ok()?.encode_utf16());
    }
    Some(JsonString::from_utf16(out))
}
fn resolve_ref<'a>(reference: &JsonString, root: &'a Value) -> Option<&'a Value> {
    let units = reference.units();
    if units != [35] && !units.starts_with(&[35, 47]) {
        return None;
    }
    let mut current = root;
    for segment in units
        .get(2..)
        .unwrap_or_default()
        .split(|u| *u == 47)
        .filter(|s| !s.is_empty())
    {
        let key = replace(
            &replace(&decode_uri_component(segment)?, "~1", "/"),
            "~0",
            "~",
        );
        current = current.as_object()?.get(&key)?;
    }
    if current.is_object() || matches!(current, Value::Bool(_)) {
        Some(current)
    } else {
        None
    }
}
fn stringify(value: &Value) -> JsonString {
    strict::stringify(value).into()
}
fn to_type(schema: &Value, context: &mut SchemaContext<'_>) -> JsonString {
    let object = match schema {
        Value::Bool(true) => return "unknown".into(),
        Value::Bool(false) => return "never".into(),
        Value::Object(o) => o,
        _ => return "unknown".into(),
    };
    if let Some(Value::String(reference)) = get(object, "$ref") {
        if context.resolving.contains(reference) || context.expansions >= MAX_REF_EXPANSIONS {
            return "unknown".into();
        }
        let Some(target) = resolve_ref(reference, context.root) else {
            return "unknown".into();
        };
        context.expansions += 1;
        context.resolving.push(reference.clone());
        let rendered = to_type(target, context);
        context.resolving.pop();
        return rendered;
    }
    if let Some(constant) = get(object, "const") {
        return stringify(constant);
    }
    if let Some(Value::Array(values)) = get(object, "enum") {
        return union(values.iter().map(stringify));
    }
    let variants = get(object, "anyOf")
        .and_then(Value::as_array)
        .or_else(|| get(object, "oneOf").and_then(Value::as_array));
    if let Some(variants) = variants {
        return union(variants.iter().map(|v| to_type(v, context)));
    }
    if let Some(Value::Array(parts)) = get(object, "allOf") {
        let parts: Vec<_> = parts
            .iter()
            .map(|v| to_type(v, context))
            .filter(|s| !s.equals_str("unknown"))
            .map(|s| {
                if contains(&s, " | ") {
                    text!("(", s, ")")
                } else {
                    s
                }
            })
            .collect();
        return if parts.is_empty() {
            "unknown".into()
        } else {
            join(parts, " & ")
        };
    }
    match get(object, "type") {
        Some(Value::Array(types)) => union(types.iter().map(|entry| {
            let mut variant = (**object).clone();
            variant.insert("type".into(), entry.clone());
            to_type(&Value::Object(variant.into()), context)
        })),
        Some(Value::String(kind)) => match kind.to_string_checked().ok().as_deref() {
            Some("string") => "string".into(),
            Some("number" | "integer") => "number".into(),
            Some("boolean") => "boolean".into(),
            Some("null") => "null".into(),
            Some("array") => array_type(object, context),
            Some("object") => object_type(object, context),
            _ => "unknown".into(),
        },
        None if has(object, "properties")
            || has(object, "additionalProperties")
            || has(object, "required") =>
        {
            object_type(object, context)
        }
        None if has(object, "items") || has(object, "prefixItems") => array_type(object, context),
        _ => "unknown".into(),
    }
}
fn array_type(schema: &Map, context: &mut SchemaContext<'_>) -> JsonString {
    if let Some(items) = get(schema, "items").filter(|v| !v.is_array()) {
        return text!("Array<", to_type(items, context), ">");
    }
    let tuple = get(schema, "prefixItems")
        .and_then(Value::as_array)
        .or_else(|| get(schema, "items").and_then(Value::as_array));
    match tuple.filter(|items| !items.is_empty()) {
        Some(items) => text!(
            "[",
            join(items.iter().map(|v| to_type(v, context)), ", "),
            "]"
        ),
        None => "unknown[]".into(),
    }
}
fn is_integer(kind: Option<&Value>) -> bool {
    kind.is_some_and(|v| {
        v.string_equals("integer")
            || v.as_array()
                .is_some_and(|a| a.iter().any(|v| v.string_equals("integer")))
    })
}
fn tag_value(value: &Value) -> Option<JsonString> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(crate::js::json_number_to_string(n).into()),
        _ => None,
    }
}
fn constraint_tags(schema: &Value) -> JsonString {
    let Some(property) = schema.as_object() else {
        return JsonString::default();
    };
    let variants = get(property, "anyOf")
        .and_then(Value::as_array)
        .or_else(|| get(property, "oneOf").and_then(Value::as_array));
    let non_null: Vec<_> = variants
        .into_iter()
        .flatten()
        .filter(|v| !v["type"].string_equals("null"))
        .collect();
    let mut branches = vec![property];
    if non_null.len() == 1
        && let Some(branch) = non_null[0].as_object()
    {
        branches.push(branch);
    }
    let schemas: Vec<_> = branches
        .into_iter()
        .map(|schema| {
            let mut schema = schema.clone();
            for (bound, exclusive) in [
                ("minimum", "exclusiveMinimum"),
                ("maximum", "exclusiveMaximum"),
            ] {
                if get(&schema, exclusive) == Some(&Value::Bool(true)) {
                    let value = schema.shift_remove(&JsonString::from(bound));
                    match value {
                        Some(value) => {
                            schema.insert(exclusive.into(), value);
                        }
                        None => {
                            schema.shift_remove(&JsonString::from(exclusive));
                        }
                    }
                }
            }
            schema
        })
        .collect();
    let mut tags = Vec::new();
    if schemas.iter().any(|s| is_integer(get(s, "type"))) {
        tags.push(JsonString::from("@integer"));
    }
    for keyword in TAG_KEYWORDS {
        let values: Vec<_> = schemas.iter().filter_map(|s| get(s, keyword)).collect();
        let lower = LOWER_BOUNDS.contains(&keyword);
        if lower || UPPER_BOUNDS.contains(&keyword) {
            let numbers: Vec<_> = values.iter().filter_map(|v| v.as_f64()).collect();
            if numbers.is_empty() {
                continue;
            }
            let bound = if lower {
                numbers.into_iter().fold(f64::NEG_INFINITY, f64::max)
            } else {
                numbers.into_iter().fold(f64::INFINITY, f64::min)
            };
            tags.push(text!("@", keyword, " ", crate::js::number_to_string(bound)));
            continue;
        }
        let mut unique = Vec::new();
        for value in values {
            if !unique.contains(&value) {
                unique.push(value);
                if let Some(text) = tag_value(value) {
                    tags.push(text!("@", keyword, " ", text));
                }
            }
        }
    }
    if schemas
        .iter()
        .any(|s| get(s, "uniqueItems") == Some(&Value::Bool(true)))
    {
        tags.push("@uniqueItems".into());
    }
    if let Some(default) = get(property, "default").filter(|v| !v.is_null()) {
        tags.push(text!("@default ", stringify(default)));
    }
    if get(property, "deprecated") == Some(&Value::Bool(true)) {
        tags.push("@deprecated".into());
    }
    join(
        tags.into_iter()
            .filter(|t| t.units().len() <= MAX_TAG_CHARS)
            .map(|tag| {
                let mut out = Vec::new();
                for u in tag.units() {
                    if line_terminator(u) {
                        out.extend(format!("\\u{u:04x}").encode_utf16());
                    } else {
                        out.push(*u);
                    }
                }
                JsonString::from_utf16(out)
            }),
        " ",
    )
}
fn object_type(schema: &Map, context: &mut SchemaContext<'_>) -> JsonString {
    let empty = Map::new();
    let properties = get(schema, "properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let required: HashSet<_> = get(schema, "required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_string)
        .collect();
    let mut names: Vec<_> = properties.keys().collect();
    names.sort_by(|a, b| a.units().cmp(b.units()));
    let mut members: Vec<_> = names
        .iter()
        .map(|name| {
            text!(
                property_key(name),
                if required.contains(name) { "" } else { "?" },
                ": ",
                to_type(&properties[*name], context),
                ";"
            )
        })
        .collect();
    match get(schema, "additionalProperties") {
        Some(Value::Bool(false)) => {}
        Some(additional) => members.push(text!(
            "[key: string]: ",
            if additional == &Value::Bool(true) {
                "unknown".into()
            } else {
                to_type(additional, context)
            },
            ";"
        )),
        None if names.is_empty() => members.push("[key: string]: unknown;".into()),
        None => {}
    }
    if members.is_empty() {
        return "{}".into();
    }
    let comments: Vec<Vec<_>> = names
        .iter()
        .map(|name| {
            let property = &properties[*name];
            let description = property["description"]
                .as_string()
                .cloned()
                .unwrap_or_default();
            let mut lines: Vec<_> = description
                .units()
                .split(line_terminator)
                .map(|line| trim(&JsonString::from_utf16(line.to_vec())))
                .filter(|s| !s.units().is_empty())
                .collect();
            if context.tags {
                let tags = constraint_tags(property);
                if !tags.units().is_empty() {
                    lines.push(tags);
                }
            }
            lines
        })
        .collect();
    if comments.iter().all(Vec::is_empty) {
        return text!("{ ", join(members, " "), " }");
    }
    let mut lines = vec!["{".into()];
    for (i, comment) in comments.iter().enumerate() {
        for line in comment {
            lines.push(text!("  // ", line));
        }
        lines.push(text!("  ", replace(&members[i], "\n", "\n  ")));
    }
    for member in members.drain(names.len()..) {
        lines.push(text!("  ", member));
    }
    lines.push("}".into());
    join(lines, "\n")
}
