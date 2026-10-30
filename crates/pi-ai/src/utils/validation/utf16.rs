//! The validation engine over strict JSON, whose strings are JavaScript UTF-16 units.
//!
//! Values, object keys, instance paths and messages keep unpaired surrogates exactly as
//! JavaScript does. The `serde_json` API in the parent module delegates here; its strings
//! are always valid UTF-8, so that conversion is lossless in both directions.
//!
//! Regular expressions (`pattern`, `patternProperties` and TypeBox property patterns) follow
//! JavaScript's `u` flag on Rust's `regex`: subjects and patterns are matched as code points,
//! with each unpaired surrogate (in the text or as a `\uXXXX` escape) mapped to one private
//! code point in U+10F800..=U+10FFFF. Only real characters from that range can collide.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

use pi_json::strict::{JsonString, Map, Value, stringify_pretty};
use regex::Regex;

// ---------------------------------------------------------------------------
// UTF-16 helpers
// ---------------------------------------------------------------------------

fn key(text: &str) -> JsonString {
    JsonString::from(text)
}

fn get<'v>(map: &'v Map, name: &str) -> Option<&'v Value> {
    map.get(&key(name))
}

fn as_object(value: &Value) -> Option<&Map> {
    value.as_object()
}

fn as_str(value: &Value) -> Option<&JsonString> {
    value.as_string()
}

fn concat<const N: usize>(parts: [&JsonString; N]) -> JsonString {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(part.units());
    }
    JsonString::from_utf16(out)
}

fn join(items: &[JsonString], separator: &str) -> JsonString {
    let separator: Vec<u16> = separator.encode_utf16().collect();
    let mut out = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.extend_from_slice(&separator);
        }
        out.extend_from_slice(item.units());
    }
    JsonString::from_utf16(out)
}

/// The text a UTF-8 regular expression or number parser sees.
fn utf8(text: &JsonString) -> Cow<'_, str> {
    match text.to_string_checked() {
        Ok(text) => Cow::Owned(text),
        Err(_) => Cow::Owned(text.to_string_lossy()),
    }
}

/// First of the 2048 private code points standing for unpaired surrogates in regexes.
const LONE_SURROGATE_BASE: u32 = 0x10F800;

fn lone_surrogate(unit: u16) -> char {
    char::from_u32(LONE_SURROGATE_BASE + u32::from(unit - 0xD800)).expect("private code point")
}

/// Code points as a `u`-flag `RegExp` sees them.
fn regex_text(text: &JsonString) -> Cow<'_, str> {
    match text.to_string_checked() {
        Ok(text) => Cow::Owned(text),
        Err(_) => Cow::Owned(
            char::decode_utf16(text.units().iter().copied())
                .map(|c| c.unwrap_or_else(|e| lone_surrogate(e.unpaired_surrogate())))
                .collect(),
        ),
    }
}

/// Rewrite `\uXXXX` escapes as `\x{...}`: a surrogate pair becomes its code point and an
/// unpaired surrogate its private stand-in. Other syntax is left to `regex`.
fn translate_pattern(source: &str) -> String {
    fn unit_at(chars: &[char], at: usize) -> Option<u16> {
        if chars.get(at) != Some(&'\\') || chars.get(at + 1) != Some(&'u') {
            return None;
        }
        let hex: String = chars.get(at + 2..at + 6)?.iter().collect();
        if hex.len() != 4 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        u16::from_str_radix(&hex, 16).ok()
    }
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut at = 0;
    while at < chars.len() {
        if let Some(unit) = unit_at(&chars, at) {
            let low = unit_at(&chars, at + 6).filter(|low| (0xDC00..0xE000).contains(low));
            let code = match (unit, low) {
                (0xD800..=0xDBFF, Some(low)) => {
                    at += 6;
                    0x10000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00)
                }
                (0xD800..=0xDFFF, _) => lone_surrogate(unit) as u32,
                _ => u32::from(unit),
            };
            out.push_str(&format!("\\x{{{code:X}}}"));
            at += 6;
        } else if chars[at] == '\\' && at + 1 < chars.len() {
            out.push(chars[at]);
            out.push(chars[at + 1]);
            at += 2;
        } else {
            out.push(chars[at]);
            at += 1;
        }
    }
    out
}

fn regex(pattern: &JsonString) -> Option<Regex> {
    Regex::new(&translate_pattern(&regex_text(pattern))).ok()
}

fn is_match(regex: &Regex, text: &JsonString) -> bool {
    regex.is_match(&regex_text(text))
}

fn escape_pointer(text: &JsonString) -> JsonString {
    let mut out = Vec::with_capacity(text.units().len());
    for &unit in text.units() {
        match unit {
            0x7e => out.extend_from_slice(&[0x7e, 0x30]),
            0x2f => out.extend_from_slice(&[0x7e, 0x31]),
            unit => out.push(unit),
        }
    }
    JsonString::from_utf16(out)
}

fn child_path(path: &JsonString, segment: &JsonString) -> JsonString {
    concat([path, &key("/"), &escape_pointer(segment)])
}

fn js_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| js_equal(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| js_equal(v, w)))
        }
        _ => a == b,
    }
}

fn is_integer(value: &Value) -> bool {
    match value {
        Value::Number(n) => {
            n.is_i64()
                || n.is_u64()
                || n.as_f64()
                    .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
        }
        _ => false,
    }
}

fn number_to_string(value: f64) -> String {
    crate::serde_util::js_number(value).to_string()
}

/// `JSON.stringify(value)` rendering of a number for error messages.
fn render_number(value: &Value) -> String {
    match value {
        Value::Number(n) => match n.as_f64() {
            Some(f) => number_to_string(f),
            None => n.to_string(),
        },
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Validator
// ---------------------------------------------------------------------------

/// A validation error in TypeBox's shape, with JavaScript strings.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub keyword: String,
    pub schema_path: JsonString,
    pub instance_path: JsonString,
    /// For `required`: the missing properties.
    pub required_properties: Vec<JsonString>,
    pub message: JsonString,
}

/// A compiled JSON Schema checker (TypeBox's `Compile(schema)`).
pub struct SchemaValidator<'a> {
    root: &'a Value,
}

impl<'a> SchemaValidator<'a> {
    pub fn new(root: &'a Value) -> Self {
        Self { root }
    }

    /// `Check(value)`.
    pub fn check(&self, value: &Value) -> bool {
        let mut errors = ErrorSink { errors: None };
        self.validate(
            self.root,
            value,
            &key("#"),
            &JsonString::default(),
            &mut errors,
            0,
        )
    }

    /// `Errors(value)`.
    pub fn errors(&self, value: &Value) -> Vec<ValidationError> {
        let mut sink = ErrorSink {
            errors: Some(Vec::new()),
        };
        self.validate(
            self.root,
            value,
            &key("#"),
            &JsonString::default(),
            &mut sink,
            0,
        );
        sink.errors.unwrap_or_default()
    }

    fn resolve_ref(&self, reference: &JsonString) -> Option<&'a Value> {
        let units = reference.units();
        let pointer = units.strip_prefix(&[0x23])?;
        if pointer.is_empty() {
            return Some(self.root);
        }
        // Each part is percent-decoded and unescaped, then the result is split again.
        let decoded = join(
            &pointer
                .split(|&unit| unit == 0x2f)
                .map(|part| unescape_pointer(&percent_decode(part)))
                .collect::<Vec<_>>(),
            "/",
        );
        // `serde_json::Value::pointer`: tokens follow a leading `/`.
        let pointed = decoded.units().strip_prefix(&[0x2f]).and_then(|rest| {
            rest.split(|&unit| unit == 0x2f)
                .try_fold(self.root, |target, token| match target {
                    Value::Object(map) => map.get(&JsonString::from_utf16(token.to_vec())),
                    Value::Array(items) => pointer_index(token).and_then(|index| items.get(index)),
                    _ => None,
                })
        });
        pointed.or_else(|| self.find_by_id(self.root, reference))
    }

    fn find_by_id(&self, schema: &'a Value, id: &JsonString) -> Option<&'a Value> {
        match schema {
            Value::Object(map) => {
                if get(map, "$id").and_then(as_str) == Some(id) {
                    return Some(schema);
                }
                map.values().find_map(|v| self.find_by_id(v, id))
            }
            Value::Array(items) => items.iter().find_map(|v| self.find_by_id(v, id)),
            _ => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn validate(
        &self,
        schema: &'a Value,
        value: &Value,
        schema_path: &JsonString,
        instance_path: &JsonString,
        sink: &mut ErrorSink,
        depth: usize,
    ) -> bool {
        if depth > 256 {
            return false;
        }
        let schema_obj = match schema {
            Value::Bool(true) => return true,
            Value::Bool(false) => {
                sink.add(
                    "boolean",
                    schema_path,
                    instance_path,
                    Vec::new(),
                    key("schema is false"),
                );
                return false;
            }
            Value::Object(map) => map,
            _ => return true,
        };
        let sub = |name: &str| concat([schema_path, &key("/"), &key(name)]);
        let mut ok = true;

        // type
        if let Some(type_value) = get(schema_obj, "type")
            && !check_type(type_value, value)
        {
            ok = false;
            let message = match type_value {
                Value::Array(types) => concat([
                    &key("must be either "),
                    &join(
                        &types
                            .iter()
                            .map(|t| as_str(t).cloned().unwrap_or_default())
                            .collect::<Vec<_>>(),
                        " or ",
                    ),
                ]),
                Value::String(t) => concat([&key("must be "), t]),
                other => key(&format!("must be {other}")),
            };
            sink.add("type", schema_path, instance_path, Vec::new(), message);
        }

        // object keywords
        if let Value::Object(object) = value {
            if let Some(Value::Array(required)) = get(schema_obj, "required") {
                let missing: Vec<JsonString> = required
                    .iter()
                    .filter_map(as_str)
                    .filter(|name| !object.contains_key(*name))
                    .cloned()
                    .collect();
                if !missing.is_empty() {
                    ok = false;
                    let message = concat([
                        &key("must have required properties "),
                        &join(&missing, ", "),
                    ]);
                    sink.add("required", schema_path, instance_path, missing, message);
                }
            }
            if let Some(additional) = get(schema_obj, "additionalProperties") {
                let properties = get(schema_obj, "properties").and_then(as_object);
                let patterns: Vec<Regex> = get(schema_obj, "patternProperties")
                    .and_then(as_object)
                    .map(|p| p.keys().filter_map(regex).collect())
                    .unwrap_or_default();
                let extra_keys: Vec<&JsonString> = object
                    .keys()
                    .filter(|name| {
                        !properties.is_some_and(|p| p.contains_key(*name))
                            && !patterns.iter().any(|r| is_match(r, name))
                    })
                    .collect();
                // Like TypeBox: each extra property is checked against the `additionalProperties`
                // schema (`false` reports "schema is false" at the property), then one
                // `additionalProperties` error at the object lists the properties that failed.
                if matches!(additional, Value::Bool(false) | Value::Object(_)) {
                    let mut failed = false;
                    for name in extra_keys {
                        if !self.validate(
                            additional,
                            &object[name],
                            &sub("additionalProperties"),
                            &child_path(instance_path, name),
                            sink,
                            depth + 1,
                        ) {
                            failed = true;
                        }
                    }
                    if failed {
                        ok = false;
                        sink.add(
                            "additionalProperties",
                            schema_path,
                            instance_path,
                            Vec::new(),
                            key("must not have additional properties"),
                        );
                    }
                }
            }
            if let Some(Value::Object(dependent)) = get(schema_obj, "dependentRequired") {
                for (property, deps) in dependent.iter() {
                    if !object.contains_key(property) {
                        continue;
                    }
                    let deps: Vec<JsonString> = deps
                        .as_array()
                        .map(|d| d.iter().filter_map(as_str).cloned().collect())
                        .unwrap_or_default();
                    if deps.iter().any(|d| !object.contains_key(d)) {
                        ok = false;
                        sink.add(
                            "dependentRequired",
                            schema_path,
                            instance_path,
                            Vec::new(),
                            concat([
                                &key("must have properties "),
                                &join(&deps, ", "),
                                &key(" when property "),
                                property,
                                &key(" is present"),
                            ]),
                        );
                    }
                }
            }
            if let Some(Value::Object(dependent)) = get(schema_obj, "dependentSchemas") {
                for (property, dep_schema) in dependent.iter() {
                    if object.contains_key(property)
                        && !self.validate(
                            dep_schema,
                            value,
                            &concat([&sub("dependentSchemas"), &key("/"), property]),
                            instance_path,
                            sink,
                            depth + 1,
                        )
                    {
                        ok = false;
                    }
                }
            }
            if let Some(Value::Object(patterns)) = get(schema_obj, "patternProperties") {
                for (pattern, pattern_schema) in patterns.iter() {
                    let Some(regex) = regex(pattern) else {
                        continue;
                    };
                    for (name, item) in object.iter() {
                        if is_match(&regex, name)
                            && !self.validate(
                                pattern_schema,
                                item,
                                &child_path(&sub("patternProperties"), pattern),
                                &child_path(instance_path, name),
                                sink,
                                depth + 1,
                            )
                        {
                            ok = false;
                        }
                    }
                }
            }
            if let Some(Value::Object(properties)) = get(schema_obj, "properties") {
                for (name, property_schema) in properties.iter() {
                    let Some(item) = object.get(name) else {
                        continue;
                    };
                    if !self.validate(
                        property_schema,
                        item,
                        &child_path(&sub("properties"), name),
                        &child_path(instance_path, name),
                        sink,
                        depth + 1,
                    ) {
                        ok = false;
                    }
                }
            }
            if let Some(names_schema) = get(schema_obj, "propertyNames") {
                let invalid: Vec<JsonString> = object
                    .keys()
                    .filter(|name| {
                        !self.check_sub(names_schema, &Value::String((*name).clone()), depth)
                    })
                    .cloned()
                    .collect();
                if !invalid.is_empty() {
                    ok = false;
                    sink.add(
                        "propertyNames",
                        schema_path,
                        instance_path,
                        Vec::new(),
                        concat([
                            &key("property names "),
                            &join(&invalid, ", "),
                            &key(" are invalid"),
                        ]),
                    );
                }
            }
            if let Some(limit) = get(schema_obj, "minProperties").and_then(Value::as_f64)
                && (object.len() as f64) < limit
            {
                ok = false;
                sink.add(
                    "minProperties",
                    schema_path,
                    instance_path,
                    Vec::new(),
                    key(&format!(
                        "must not have fewer than {} properties",
                        number_to_string(limit)
                    )),
                );
            }
            if let Some(limit) = get(schema_obj, "maxProperties").and_then(Value::as_f64)
                && (object.len() as f64) > limit
            {
                ok = false;
                sink.add(
                    "maxProperties",
                    schema_path,
                    instance_path,
                    Vec::new(),
                    key(&format!(
                        "must not have more than {} properties",
                        number_to_string(limit)
                    )),
                );
            }
        }

        // array keywords
        if let Value::Array(items) = value {
            if let Some(contains) = get(schema_obj, "contains") {
                let count = items
                    .iter()
                    .filter(|item| self.check_sub(contains, item, depth))
                    .count();
                let min_contains = get(schema_obj, "minContains")
                    .and_then(Value::as_f64)
                    .unwrap_or(1.0);
                if (count as f64) < min_contains {
                    ok = false;
                    sink.add(
                        "contains",
                        schema_path,
                        instance_path,
                        Vec::new(),
                        key("must contain at least 1 valid item"),
                    );
                }
                if let Some(max) = get(schema_obj, "maxContains").and_then(Value::as_f64)
                    && count as f64 > max
                {
                    ok = false;
                    sink.add(
                        "maxContains",
                        schema_path,
                        instance_path,
                        Vec::new(),
                        key(&format!(
                            "must not contain more than {} matching items",
                            number_to_string(max)
                        )),
                    );
                }
            }
            let index_path = |index: usize| child_path(instance_path, &key(&index.to_string()));
            let prefix_len = match get(schema_obj, "prefixItems") {
                Some(Value::Array(prefix)) => {
                    for (index, (item_schema, item)) in prefix.iter().zip(items.iter()).enumerate()
                    {
                        if !self.validate(
                            item_schema,
                            item,
                            &concat([&sub("prefixItems"), &key(&format!("/{index}"))]),
                            &index_path(index),
                            sink,
                            depth + 1,
                        ) {
                            ok = false;
                        }
                    }
                    prefix.len()
                }
                _ => 0,
            };
            match get(schema_obj, "items") {
                Some(Value::Array(tuple)) => {
                    for (index, (item_schema, item)) in tuple.iter().zip(items.iter()).enumerate() {
                        if !self.validate(
                            item_schema,
                            item,
                            &concat([&sub("items"), &key(&format!("/{index}"))]),
                            &index_path(index),
                            sink,
                            depth + 1,
                        ) {
                            ok = false;
                        }
                    }
                    if let Some(additional) = get(schema_obj, "additionalItems") {
                        for (index, item) in items.iter().enumerate().skip(tuple.len()) {
                            if !self.validate(
                                additional,
                                item,
                                &sub("additionalItems"),
                                &index_path(index),
                                sink,
                                depth + 1,
                            ) {
                                ok = false;
                            }
                        }
                    }
                }
                Some(item_schema) => {
                    for (index, item) in items.iter().enumerate().skip(prefix_len) {
                        if !self.validate(
                            item_schema,
                            item,
                            &sub("items"),
                            &index_path(index),
                            sink,
                            depth + 1,
                        ) {
                            ok = false;
                        }
                    }
                }
                None => {}
            }
            if let Some(limit) = get(schema_obj, "maxItems").and_then(Value::as_f64)
                && items.len() as f64 > limit
            {
                ok = false;
                sink.add(
                    "maxItems",
                    schema_path,
                    instance_path,
                    Vec::new(),
                    key(&format!(
                        "must not have more than {} items",
                        number_to_string(limit)
                    )),
                );
            }
            if let Some(limit) = get(schema_obj, "minItems").and_then(Value::as_f64)
                && (items.len() as f64) < limit
            {
                ok = false;
                sink.add(
                    "minItems",
                    schema_path,
                    instance_path,
                    Vec::new(),
                    key(&format!(
                        "must not have fewer than {} items",
                        number_to_string(limit)
                    )),
                );
            }
            if get(schema_obj, "uniqueItems") == Some(&Value::Bool(true)) {
                let duplicate = items
                    .iter()
                    .enumerate()
                    .any(|(i, a)| items.iter().skip(i + 1).any(|b| js_equal(a, b)));
                if duplicate {
                    ok = false;
                    sink.add(
                        "uniqueItems",
                        schema_path,
                        instance_path,
                        Vec::new(),
                        key("must not have duplicate items"),
                    );
                }
            }
        }

        // string keywords
        if let Value::String(text) = value {
            let length = text.units().len() as f64;
            if let Some(limit) = get(schema_obj, "maxLength").and_then(Value::as_f64)
                && length > limit
            {
                ok = false;
                sink.add(
                    "maxLength",
                    schema_path,
                    instance_path,
                    Vec::new(),
                    key(&format!(
                        "must not have more than {} characters",
                        number_to_string(limit)
                    )),
                );
            }
            if let Some(limit) = get(schema_obj, "minLength").and_then(Value::as_f64)
                && length < limit
            {
                ok = false;
                sink.add(
                    "minLength",
                    schema_path,
                    instance_path,
                    Vec::new(),
                    key(&format!(
                        "must not have fewer than {} characters",
                        number_to_string(limit)
                    )),
                );
            }
            if let Some(pattern) = get(schema_obj, "pattern").and_then(as_str)
                && let Some(regex) = regex(pattern)
                && !is_match(&regex, text)
            {
                ok = false;
                sink.add(
                    "pattern",
                    schema_path,
                    instance_path,
                    Vec::new(),
                    concat([&key("must match pattern \""), pattern, &key("\"")]),
                );
            }
        }

        // number keywords
        if let Some(number) = value.as_f64().filter(|_| value.is_number()) {
            type Check = (&'static str, &'static str, fn(f64, f64) -> bool);
            let checks: [Check; 4] = [
                ("exclusiveMaximum", "<", |v, l| v < l),
                ("exclusiveMinimum", ">", |v, l| v > l),
                ("maximum", "<=", |v, l| v <= l),
                ("minimum", ">=", |v, l| v >= l),
            ];
            for (keyword, comparison, predicate) in checks {
                if let Some(limit) = get(schema_obj, keyword).filter(|l| l.is_number()) {
                    let limit_f = limit.as_f64().unwrap_or(0.0);
                    if !predicate(number, limit_f) {
                        ok = false;
                        sink.add(
                            keyword,
                            schema_path,
                            instance_path,
                            Vec::new(),
                            key(&format!("must be {comparison} {}", render_number(limit))),
                        );
                    }
                }
            }
            if let Some(multiple) = get(schema_obj, "multipleOf").and_then(Value::as_f64)
                && multiple != 0.0
            {
                let quotient = number / multiple;
                if (quotient - quotient.round()).abs() > 1e-9 {
                    ok = false;
                    sink.add(
                        "multipleOf",
                        schema_path,
                        instance_path,
                        Vec::new(),
                        key(&format!(
                            "must be multiple of {}",
                            number_to_string(multiple)
                        )),
                    );
                }
            }
        }

        // $ref
        if let Some(reference) = get(schema_obj, "$ref").and_then(as_str) {
            match self.resolve_ref(reference) {
                Some(target) => {
                    if !self.validate(target, value, &sub("$ref"), instance_path, sink, depth + 1) {
                        ok = false;
                    }
                }
                None => ok = false,
            }
        }

        if let Some(constant) = get(schema_obj, "const")
            && !js_equal(constant, value)
        {
            ok = false;
            sink.add(
                "const",
                schema_path,
                instance_path,
                Vec::new(),
                key("must be equal to constant"),
            );
        }
        if let Some(Value::Array(options)) = get(schema_obj, "enum")
            && !options.iter().any(|o| js_equal(o, value))
        {
            ok = false;
            sink.add(
                "enum",
                schema_path,
                instance_path,
                Vec::new(),
                key("must be equal to one of the allowed values"),
            );
        }
        if let Some(if_schema) = get(schema_obj, "if") {
            let (branch, keyword) = if self.check_sub(if_schema, value, depth) {
                (get(schema_obj, "then"), "then")
            } else {
                (get(schema_obj, "else"), "else")
            };
            if let Some(branch) = branch
                && !self.check_sub(branch, value, depth)
            {
                ok = false;
                sink.add(
                    "if",
                    schema_path,
                    instance_path,
                    Vec::new(),
                    key(&format!("must match \"{keyword}\" schema")),
                );
            }
        }
        if let Some(not_schema) = get(schema_obj, "not")
            && self.check_sub(not_schema, value, depth)
        {
            ok = false;
            sink.add(
                "not",
                schema_path,
                instance_path,
                Vec::new(),
                key("must not be valid"),
            );
        }
        if let Some(Value::Array(all)) = get(schema_obj, "allOf") {
            for (index, member) in all.iter().enumerate() {
                if !self.validate(
                    member,
                    value,
                    &concat([&sub("allOf"), &key(&format!("/{index}"))]),
                    instance_path,
                    sink,
                    depth + 1,
                ) {
                    ok = false;
                }
            }
        }
        if let Some(Value::Array(any)) = get(schema_obj, "anyOf") {
            let mut failed: Vec<Vec<ValidationError>> = Vec::new();
            let mut matched = false;
            for (index, member) in any.iter().enumerate() {
                let mut nested = ErrorSink {
                    errors: sink.errors.as_ref().map(|_| Vec::new()),
                };
                if self.validate(
                    member,
                    value,
                    &concat([&sub("anyOf"), &key(&format!("/{index}"))]),
                    instance_path,
                    &mut nested,
                    depth + 1,
                ) {
                    matched = true;
                    break;
                }
                failed.push(nested.errors.unwrap_or_default());
            }
            if !matched {
                ok = false;
                for errors in failed {
                    for error in errors {
                        sink.push(error);
                    }
                }
                sink.add(
                    "anyOf",
                    schema_path,
                    instance_path,
                    Vec::new(),
                    key("must match a schema in anyOf"),
                );
            }
        }
        if let Some(Value::Array(one)) = get(schema_obj, "oneOf") {
            let mut failed: Vec<Vec<ValidationError>> = Vec::new();
            let mut passing = 0;
            for (index, member) in one.iter().enumerate() {
                let mut nested = ErrorSink {
                    errors: sink.errors.as_ref().map(|_| Vec::new()),
                };
                if self.validate(
                    member,
                    value,
                    &concat([&sub("oneOf"), &key(&format!("/{index}"))]),
                    instance_path,
                    &mut nested,
                    depth + 1,
                ) {
                    passing += 1;
                } else {
                    failed.push(nested.errors.unwrap_or_default());
                }
            }
            if passing != 1 {
                ok = false;
                if passing == 0 {
                    for errors in failed {
                        for error in errors {
                            sink.push(error);
                        }
                    }
                }
                sink.add(
                    "oneOf",
                    schema_path,
                    instance_path,
                    Vec::new(),
                    key("must match exactly one schema in oneOf"),
                );
            }
        }
        ok
    }

    fn check_sub(&self, schema: &'a Value, value: &Value, depth: usize) -> bool {
        let mut sink = ErrorSink { errors: None };
        self.validate(
            schema,
            value,
            &key("#"),
            &JsonString::default(),
            &mut sink,
            depth + 1,
        )
    }
}

/// A JSON Pointer array index: `0` or digits without a leading zero.
fn pointer_index(token: &[u16]) -> Option<usize> {
    let text = String::from_utf16(token).ok()?;
    if text.starts_with('+') || (text.starts_with('0') && text.len() > 1) {
        return None;
    }
    text.parse().ok()
}

fn unescape_pointer(part: &JsonString) -> JsonString {
    let mut out = Vec::with_capacity(part.units().len());
    let mut units = part.units().iter().copied().peekable();
    // `replace("~1", "/").replace("~0", "~")`: `~01` becomes `~1`, not `/`.
    while let Some(unit) = units.next() {
        if unit == 0x7e && units.peek() == Some(&0x31) {
            units.next();
            out.push(0x2f);
        } else {
            out.push(unit);
        }
    }
    let mut result = Vec::with_capacity(out.len());
    let mut units = out.into_iter().peekable();
    while let Some(unit) = units.next() {
        if unit == 0x7e && units.peek() == Some(&0x30) {
            units.next();
            result.push(0x7e);
        } else {
            result.push(unit);
        }
    }
    JsonString::from_utf16(result)
}

/// Percent-decode the UTF-8 bytes of `%XX` runs, keeping every other unit.
fn percent_decode(part: &[u16]) -> JsonString {
    fn hex(unit: u16) -> Option<u8> {
        char::from_u32(unit as u32)?
            .to_digit(16)
            .map(|digit| digit as u8)
    }
    let mut out = Vec::with_capacity(part.len());
    let mut bytes = Vec::new();
    let mut i = 0;
    while i < part.len() {
        if part[i] == 0x25
            && i + 2 < part.len()
            && let (Some(high), Some(low)) = (hex(part[i + 1]), hex(part[i + 2]))
        {
            bytes.push(high << 4 | low);
            i += 3;
            continue;
        }
        if !bytes.is_empty() {
            out.extend(String::from_utf8_lossy(&std::mem::take(&mut bytes)).encode_utf16());
        }
        out.push(part[i]);
        i += 1;
    }
    if !bytes.is_empty() {
        out.extend(String::from_utf8_lossy(&bytes).encode_utf16());
    }
    JsonString::from_utf16(out)
}

struct ErrorSink {
    errors: Option<Vec<ValidationError>>,
}

impl ErrorSink {
    fn add(
        &mut self,
        keyword: &str,
        schema_path: &JsonString,
        instance_path: &JsonString,
        required: Vec<JsonString>,
        message: JsonString,
    ) {
        if let Some(errors) = &mut self.errors {
            errors.push(ValidationError {
                keyword: keyword.to_string(),
                schema_path: schema_path.clone(),
                instance_path: instance_path.clone(),
                required_properties: required,
                message,
            });
        }
    }

    fn push(&mut self, error: ValidationError) {
        if let Some(errors) = &mut self.errors {
            errors.push(error);
        }
    }
}

fn check_type(type_value: &Value, value: &Value) -> bool {
    match type_value {
        Value::String(t) => matches_json_type(value, &utf8(t)),
        Value::Array(types) => types
            .iter()
            .filter_map(as_str)
            .any(|t| matches_json_type(value, &utf8(t))),
        _ => true,
    }
}

fn matches_json_type(value: &Value, type_name: &str) -> bool {
    match type_name {
        "number" => value.is_number(),
        "integer" => is_integer(value),
        "boolean" => matches!(value, Value::Bool(_)),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

/// Validate `value` against `schema` (TypeBox `Compile(schema).Check(value)`).
pub fn check_schema(schema: &Value, value: &Value) -> bool {
    SchemaValidator::new(schema).check(value)
}

// ---------------------------------------------------------------------------
// TypeBox `Value.Convert` (typebox 1.3.27, `build/value/convert`)
// ---------------------------------------------------------------------------

/// The TypeBox kind of a schema, inferred from the JSON TypeBox emits for it (the `~kind`
/// marker itself is non-enumerable and does not survive JSON).
enum TypeBoxKind<'a> {
    Array(&'a Value),
    Tuple(&'a [Value]),
    Boolean,
    Integer,
    Number,
    String,
    Null,
    Literal(&'a Value),
    Enum(&'a [Value]),
    Object,
    Record(&'a Map),
    Union(&'a [Value]),
    Intersect(&'a [Value]),
    Ref(&'a JsonString),
    Cyclic(&'a Map, &'a JsonString),
    Other,
}

fn typebox_kind(schema: &Value) -> TypeBoxKind<'_> {
    let Some(map) = schema.as_object() else {
        return TypeBoxKind::Other;
    };
    if let (Some(Value::Object(defs)), Some(Value::String(reference))) =
        (get(map, "$defs"), get(map, "$ref"))
    {
        return TypeBoxKind::Cyclic(defs, reference);
    }
    if let Some(Value::String(reference)) = get(map, "$ref") {
        return TypeBoxKind::Ref(reference);
    }
    if let Some(Value::Array(members)) = get(map, "anyOf") {
        return TypeBoxKind::Union(members);
    }
    if let Some(Value::Array(members)) = get(map, "allOf") {
        return TypeBoxKind::Intersect(members);
    }
    if let Some(constant) = get(map, "const") {
        return TypeBoxKind::Literal(constant);
    }
    if let (None, Some(Value::Array(values))) = (get(map, "type"), get(map, "enum")) {
        return TypeBoxKind::Enum(values);
    }
    match get(map, "type").and_then(Value::to_utf8).as_deref() {
        Some("array") => match get(map, "items") {
            Some(Value::Array(items)) => TypeBoxKind::Tuple(items),
            Some(items @ Value::Object(_)) => TypeBoxKind::Array(items),
            _ => TypeBoxKind::Other,
        },
        Some("boolean") => TypeBoxKind::Boolean,
        Some("integer") => TypeBoxKind::Integer,
        Some("number") => TypeBoxKind::Number,
        Some("string") => TypeBoxKind::String,
        Some("null") => TypeBoxKind::Null,
        Some("object") => match get(map, "patternProperties") {
            Some(Value::Object(patterns)) if !map.contains_key(&key("properties")) => {
                TypeBoxKind::Record(patterns)
            }
            _ => TypeBoxKind::Object,
        },
        _ => TypeBoxKind::Other,
    }
}

fn object(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Object(Arc::new(
        entries
            .into_iter()
            .map(|(name, value)| (key(name), value))
            .collect(),
    ))
}

/// `Guard.IsNumber`: a finite number.
fn finite_number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|n| n.is_finite())
}

/// `TryNumber(value)`.
fn try_number(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(_) => finite_number(value),
        Value::Null => Some(0.0),
        Value::String(text) => {
            let text = utf8(text);
            let text = text.as_ref();
            // `+value`
            if let Some(n) = js_number_from_string(text).filter(|n| n.is_finite()) {
                return Some(n);
            }
            match text.to_lowercase().as_str() {
                "false" => return Some(0.0),
                "true" => return Some(1.0),
                _ => {}
            }
            // TryBigInt: `-?(0|[1-9]\d*)n`, `-?(0|[1-9]\d*)\.\d+`, `-?(0|[1-9]\d*)`.
            let digits = text.strip_suffix('n').unwrap_or(text);
            let (negative, unsigned) = match digits.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, digits),
            };
            let integer_part = if text.ends_with('n') {
                unsigned
            } else {
                unsigned
                    .split_once('.')
                    .map(|(i, f)| {
                        if !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()) {
                            i
                        } else {
                            ""
                        }
                    })
                    .unwrap_or(unsigned)
            };
            let canonical = integer_part == "0"
                || (!integer_part.is_empty()
                    && !integer_part.starts_with('0')
                    && integer_part.bytes().all(|b| b.is_ascii_digit()));
            if !canonical {
                return None;
            }
            let magnitude: f64 = integer_part.parse().ok()?;
            // Number(bigint) only within the safe integer range.
            (magnitude <= 9_007_199_254_740_991.0).then_some(if negative {
                -magnitude
            } else {
                magnitude
            })
        }
        _ => None,
    }
}

/// `TryBoolean(value)`.
fn try_boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => match n.as_f64() {
            Some(0.0) => Some(false),
            Some(1.0) => Some(true),
            _ => None,
        },
        Value::Null => Some(false),
        Value::String(text) => match utf8(text).to_lowercase().as_str() {
            "false" => Some(false),
            "true" => Some(true),
            _ if text.equals_str("0") => Some(false),
            _ if text.equals_str("1") => Some(true),
            _ => None,
        },
        _ => None,
    }
}

/// `TryString(value)`.
fn try_string(value: &Value) -> Option<JsonString> {
    match value {
        Value::Bool(_) | Value::Number(_) => Some(js_to_string(value)),
        Value::Null => Some(key("null")),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// `TryNull(value)`.
fn try_null(value: &Value) -> bool {
    match value {
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Null => true,
        Value::String(text) => {
            let lower = utf8(text).to_lowercase();
            lower == "undefined"
                || lower == "null"
                || text.units().is_empty()
                || text.equals_str("0")
        }
        _ => false,
    }
}

/// `new RegExp(`^${key}$`).test(candidate)` as TypeBox's `EntriesRegExp` matches property keys.
fn key_pattern_matches(pattern: &JsonString, candidate: &JsonString) -> bool {
    // Property names are almost always plain; those with regex syntax go through `regex`.
    let text = utf8(pattern);
    if !text.contains(|c: char| "\\^$.|?*+()[]{}".contains(c)) {
        return pattern == candidate;
    }
    match Regex::new(&format!(
        "^(?:{})$",
        translate_pattern(&regex_text(pattern))
    )) {
        Ok(re) => is_match(&re, candidate),
        Err(_) => pattern == candidate,
    }
}

/// `Value.Convert(schema, value)` of TypeBox 1.3.27 for a schema built with TypeBox.
pub fn typebox_convert(schema: &Value, value: Value) -> Value {
    typebox_convert_in(&Map::new(), schema, value)
}

fn typebox_convert_in(context: &Map, schema: &Value, value: Value) -> Value {
    match typebox_kind(schema) {
        TypeBoxKind::Array(items) => {
            let elements = match value {
                Value::Array(elements) => Arc::unwrap_or_clone(elements),
                other => vec![other],
            };
            Value::Array(Arc::new(
                elements
                    .into_iter()
                    .map(|e| typebox_convert_in(context, items, e))
                    .collect(),
            ))
        }
        TypeBoxKind::Tuple(items) => match value {
            Value::Array(elements) => {
                let mut elements = Arc::unwrap_or_clone(elements);
                for (index, item) in items.iter().enumerate().take(elements.len()) {
                    let current = std::mem::take(&mut elements[index]);
                    elements[index] = typebox_convert_in(context, item, current);
                }
                Value::Array(Arc::new(elements))
            }
            other => other,
        },
        TypeBoxKind::Boolean => try_boolean(&value).map(Value::Bool).unwrap_or(value),
        TypeBoxKind::Integer => try_number(&value)
            .map(|n| number_value(n.trunc()))
            .unwrap_or(value),
        TypeBoxKind::Number => try_number(&value).map(number_value).unwrap_or(value),
        TypeBoxKind::String => try_string(&value).map(Value::String).unwrap_or(value),
        TypeBoxKind::Null => {
            if try_null(&value) {
                Value::Null
            } else {
                value
            }
        }
        TypeBoxKind::Literal(constant) => convert_literal(constant, value),
        TypeBoxKind::Enum(values) => {
            // Evaluate(Enum) is a union of literals.
            let members: Vec<Value> = values
                .iter()
                .map(|v| object([("const", v.clone())]))
                .collect();
            let union = object([("anyOf", Value::Array(Arc::new(members.clone())))]);
            convert_union(context, &members, &union, value)
        }
        TypeBoxKind::Object => match value {
            Value::Object(object) => {
                let mut object = Arc::unwrap_or_clone(object);
                let map = schema.as_object().expect("object kind");
                let properties = get(map, "properties").and_then(as_object);
                let keys: Vec<JsonString> = object.keys().cloned().collect();
                if let Some(properties) = properties {
                    for (pattern, property) in properties.iter() {
                        for name in &keys {
                            if key_pattern_matches(pattern, name)
                                && let Some(current) = object.get(name).cloned()
                            {
                                object.insert(
                                    name.clone(),
                                    typebox_convert_in(context, property, current),
                                );
                            }
                        }
                    }
                }
                if let Some(additional @ Value::Object(_)) = get(map, "additionalProperties") {
                    let patterns: Vec<&JsonString> =
                        properties.map(|p| p.keys().collect()).unwrap_or_default();
                    convert_additional(context, &patterns, additional, &mut object, &keys);
                }
                Value::Object(Arc::new(object))
            }
            other => other,
        },
        TypeBoxKind::Record(patterns) => match value {
            Value::Object(object) => {
                let mut object = Arc::unwrap_or_clone(object);
                let keys: Vec<JsonString> = object.keys().cloned().collect();
                for (pattern, property) in patterns.iter() {
                    for name in &keys {
                        if key_pattern_matches(pattern, name)
                            && let Some(current) = object.get(name).cloned()
                        {
                            object.insert(
                                name.clone(),
                                typebox_convert_in(context, property, current),
                            );
                        }
                    }
                }
                if let Some(additional @ Value::Object(_)) = schema
                    .as_object()
                    .and_then(|map| get(map, "additionalProperties"))
                {
                    let pattern_keys: Vec<&JsonString> = patterns.keys().collect();
                    convert_additional(context, &pattern_keys, additional, &mut object, &keys);
                }
                Value::Object(Arc::new(object))
            }
            other => other,
        },
        TypeBoxKind::Union(members) => convert_union(context, members, schema, value),
        TypeBoxKind::Intersect(members) => {
            // Evaluate(Intersect) of objects: one object with the members' properties.
            let mut properties = Map::new();
            for member in members {
                let member_properties = member
                    .as_object()
                    .and_then(|m| get(m, "properties"))
                    .and_then(as_object);
                let is_object = member
                    .as_object()
                    .and_then(|m| get(m, "type"))
                    .is_some_and(|t| t.string_equals("object"));
                match member_properties {
                    Some(member_properties) if is_object => {
                        for (name, property) in member_properties.iter() {
                            properties.insert(name.clone(), property.clone());
                        }
                    }
                    _ => return value,
                }
            }
            let merged = object([
                ("type", Value::from("object")),
                ("properties", Value::Object(Arc::new(properties))),
            ]);
            typebox_convert_in(context, &merged, value)
        }
        TypeBoxKind::Ref(reference) => match context.get(reference) {
            Some(target) => typebox_convert_in(context, target, value),
            None => value,
        },
        TypeBoxKind::Cyclic(defs, reference) => {
            let mut merged = context.clone();
            for (name, def) in defs.iter() {
                merged.insert(name.clone(), def.clone());
            }
            match merged.get(reference).cloned() {
                Some(target) => typebox_convert_in(&merged, &target, value),
                None => value,
            }
        }
        TypeBoxKind::Other => value,
    }
}

fn convert_additional(
    context: &Map,
    patterns: &[&JsonString],
    additional: &Value,
    object: &mut Map,
    keys: &[JsonString],
) {
    for pattern in patterns {
        for name in keys {
            if !key_pattern_matches(pattern, name)
                && let Some(current) = object.get(name).cloned()
            {
                object.insert(
                    name.clone(),
                    typebox_convert_in(context, additional, current),
                );
            }
        }
    }
}

/// `FromLiteral`: convert to the literal's type when the result equals the constant.
fn convert_literal(constant: &Value, value: Value) -> Value {
    if js_equal(constant, &value) {
        return value;
    }
    let converted = match constant {
        Value::Bool(_) => try_boolean(&value).map(Value::Bool),
        Value::Number(_) => try_number(&value).map(number_value),
        Value::String(_) => try_string(&value).map(Value::String),
        _ => None,
    };
    match converted {
        Some(converted) if js_equal(constant, &converted) => converted,
        _ => value,
    }
}

/// `FromUnion`: keep a value a member accepts, else the first member conversion the union accepts.
fn convert_union(context: &Map, members: &[Value], union: &Value, value: Value) -> Value {
    if members.iter().any(|member| check_schema(member, &value)) {
        return value;
    }
    for member in members {
        let candidate = typebox_convert_in(context, member, value.clone());
        if check_schema(union, &candidate) {
            return candidate;
        }
    }
    value
}

// ---------------------------------------------------------------------------
// Coercion (plain JSON schemas)
// ---------------------------------------------------------------------------

fn get_schema_types(schema: &Map) -> Vec<String> {
    match get(schema, "type") {
        Some(Value::String(t)) => vec![utf8(t).into_owned()],
        Some(Value::Array(types)) => types
            .iter()
            .filter_map(as_str)
            .map(|t| utf8(t).into_owned())
            .collect(),
        _ => Vec::new(),
    }
}

/// JavaScript `Number(text)` for strings.
fn js_number_from_string(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Some(0.0);
    }
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return u64::from_str_radix(hex, 16).ok().map(|v| v as f64);
    }
    if let Some(bin) = trimmed
        .strip_prefix("0b")
        .or_else(|| trimmed.strip_prefix("0B"))
    {
        return u64::from_str_radix(bin, 2).ok().map(|v| v as f64);
    }
    if let Some(oct) = trimmed
        .strip_prefix("0o")
        .or_else(|| trimmed.strip_prefix("0O"))
    {
        return u64::from_str_radix(oct, 8).ok().map(|v| v as f64);
    }
    match trimmed {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    let valid = trimmed
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    if !valid {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

fn number_value(value: f64) -> Value {
    crate::serde_util::js_number(value).into()
}

fn js_to_string(value: &Value) -> JsonString {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => key(&match n.as_f64() {
            Some(f) if n.is_f64() => number_to_string(f),
            _ => n.to_string(),
        }),
        Value::Bool(b) => key(&b.to_string()),
        other => key(&other.to_string()),
    }
}

fn is_blank(text: &JsonString) -> bool {
    utf8(text).trim().is_empty()
}

fn coerce_primitive_by_type(value: &Value, type_name: &str) -> Value {
    match type_name {
        "number" => match value {
            Value::Null => number_value(0.0),
            Value::String(s) if !is_blank(s) => match js_number_from_string(&utf8(s)) {
                Some(parsed) if parsed.is_finite() => number_value(parsed),
                _ => value.clone(),
            },
            Value::Bool(b) => number_value(if *b { 1.0 } else { 0.0 }),
            _ => value.clone(),
        },
        "integer" => match value {
            Value::Null => number_value(0.0),
            Value::String(s) if !is_blank(s) => match js_number_from_string(&utf8(s)) {
                Some(parsed) if parsed.is_finite() && parsed.fract() == 0.0 => number_value(parsed),
                _ => value.clone(),
            },
            Value::Bool(b) => number_value(if *b { 1.0 } else { 0.0 }),
            _ => value.clone(),
        },
        "boolean" => match value {
            Value::Null => Value::Bool(false),
            Value::String(s) if s.equals_str("true") => Value::Bool(true),
            Value::String(s) if s.equals_str("false") => Value::Bool(false),
            Value::Number(n) if n.as_f64() == Some(1.0) => Value::Bool(true),
            Value::Number(n) if n.as_f64() == Some(0.0) => Value::Bool(false),
            _ => value.clone(),
        },
        "string" => match value {
            Value::Null => Value::String(JsonString::default()),
            Value::Number(_) | Value::Bool(_) => Value::String(js_to_string(value)),
            _ => value.clone(),
        },
        "null" => match value {
            Value::String(s) if s.units().is_empty() => Value::Null,
            Value::Number(n) if n.as_f64() == Some(0.0) => Value::Null,
            Value::Bool(false) => Value::Null,
            _ => value.clone(),
        },
        _ => value.clone(),
    }
}

/// Identity comparison for coercion: JS `candidate !== nextValue` for primitives.
fn same_primitive(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Object(_), Value::Object(_)) | (Value::Array(_), Value::Array(_)) => true,
        _ => a == b,
    }
}

fn apply_schema_object_coercion(value: &mut Map, schema: &Map) {
    let properties = get(schema, "properties").and_then(as_object);
    let defined: HashSet<&JsonString> = properties.map(|p| p.keys().collect()).unwrap_or_default();
    if let Some(properties) = properties {
        for (name, property_schema) in properties.iter() {
            if let Some(current) = value.get(name).cloned() {
                value.insert(
                    name.clone(),
                    coerce_with_json_schema(current, property_schema),
                );
            }
        }
    }
    if let Some(additional @ Value::Object(_)) = get(schema, "additionalProperties") {
        let keys: Vec<JsonString> = value
            .keys()
            .filter(|k| !defined.contains(k))
            .cloned()
            .collect();
        for name in keys {
            let current = value[&name].clone();
            value.insert(name, coerce_with_json_schema(current, additional));
        }
    }
}

fn apply_schema_array_coercion(value: &mut [Value], schema: &Map) {
    match get(schema, "items") {
        Some(Value::Array(item_schemas)) => {
            for (index, item) in value.iter_mut().enumerate() {
                if let Some(item_schema) = item_schemas.get(index) {
                    *item = coerce_with_json_schema(std::mem::take(item), item_schema);
                }
            }
        }
        Some(item_schema @ Value::Object(_)) => {
            for item in value.iter_mut() {
                *item = coerce_with_json_schema(std::mem::take(item), item_schema);
            }
        }
        _ => {}
    }
}

fn coerce_with_union_schema(value: Value, schemas: &[Value]) -> Value {
    for schema in schemas {
        if check_schema(schema, &value) {
            return value;
        }
    }
    for schema in schemas {
        let coerced = coerce_with_json_schema(value.clone(), schema);
        if check_schema(schema, &coerced) {
            return coerced;
        }
    }
    value
}

/// Coerce a value toward a plain JSON schema using AJV-compatible primitive rules.
pub fn coerce_with_json_schema(value: Value, schema: &Value) -> Value {
    let Some(schema) = schema.as_object() else {
        return value;
    };
    let mut next = value;
    if let Some(Value::Array(all)) = get(schema, "allOf") {
        for nested in all.iter() {
            next = coerce_with_json_schema(next, nested);
        }
    }
    if let Some(Value::Array(any)) = get(schema, "anyOf") {
        next = coerce_with_union_schema(next, any);
    }
    if let Some(Value::Array(one)) = get(schema, "oneOf") {
        next = coerce_with_union_schema(next, one);
    }
    let types = get_schema_types(schema);
    let matches_union_member = types.len() > 1 && types.iter().any(|t| matches_json_type(&next, t));
    if !types.is_empty() && !matches_union_member {
        for type_name in &types {
            let candidate = coerce_primitive_by_type(&next, type_name);
            if !same_primitive(&candidate, &next) {
                next = candidate;
                break;
            }
        }
    }
    if types.iter().any(|t| t == "object")
        && let Some(map) = next.as_object_mut()
    {
        apply_schema_object_coercion(map, schema);
    }
    if types.iter().any(|t| t == "array")
        && let Some(items) = next.as_array_mut()
    {
        apply_schema_array_coercion(items, schema);
    }
    next
}

fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    let Some(schema_obj) = schema.as_object() else {
        return;
    };
    if let Some(items) = value.as_array_mut() {
        match get(schema_obj, "items") {
            Some(Value::Array(item_schemas)) => {
                for (index, item) in items.iter_mut().enumerate() {
                    if let Some(item_schema) = item_schemas.get(index) {
                        normalize_optional_nulls(item, item_schema);
                    }
                }
            }
            Some(item_schema) => {
                for item in items.iter_mut() {
                    normalize_optional_nulls(item, item_schema);
                }
            }
            None => {}
        }
        return;
    }
    let Some(Value::Object(properties)) = get(schema_obj, "properties") else {
        return;
    };
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let required: HashSet<&JsonString> = get(schema_obj, "required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(as_str).collect())
        .unwrap_or_default();
    for (name, property_schema) in properties.iter() {
        let Some(current) = object.get_mut(name) else {
            continue;
        };
        let is_ref = property_schema
            .as_object()
            .and_then(|p| get(p, "$ref"))
            .is_some_and(Value::is_string);
        if current.is_null()
            && !required.contains(name)
            && !is_ref
            && !check_schema(property_schema, &Value::Null)
        {
            object.shift_remove(name);
        } else {
            normalize_optional_nulls(current, property_schema);
        }
    }
}

fn format_validation_path(error: &ValidationError) -> JsonString {
    let units = error.instance_path.units();
    let base: Vec<u16> = units
        .strip_prefix(&[0x2f])
        .unwrap_or(units)
        .iter()
        .map(|&u| if u == 0x2f { 0x2e } else { u })
        .collect();
    let base = JsonString::from_utf16(base);
    if error.keyword == "required"
        && let Some(required) = error.required_properties.first()
    {
        return if base.units().is_empty() {
            required.clone()
        } else {
            concat([&base, &key("."), required])
        };
    }
    if base.units().is_empty() {
        key("root")
    } else {
        base
    }
}

/// Validates tool call arguments against a schema, returning the coerced arguments or
/// `validateToolArguments`' error message.
///
/// `arguments_error` is the call's JSON parse error, reported before validation.
pub fn validate_tool_arguments(
    name: &JsonString,
    parameters: &Value,
    typebox: bool,
    arguments: &Value,
    arguments_error: Option<&JsonString>,
) -> Result<Value, JsonString> {
    if let Some(arguments_error) = arguments_error {
        return Err(concat([
            &key("Invalid arguments for tool \""),
            name,
            &key("\": "),
            arguments_error,
        ]));
    }
    let mut args = arguments.clone();
    normalize_optional_nulls(&mut args, parameters);
    // TS: `Value.Convert(tool.parameters, args)` converts schemas built with TypeBox (it does nothing
    // for plain JSON schemas, which carry no `~kind`); the JSON Schema coercion follows for every
    // schema without the legacy TypeBox 0.x `Kind` symbol.
    if typebox {
        args = typebox_convert(parameters, args);
    }
    let args = coerce_with_json_schema(args, parameters);

    let validator = SchemaValidator::new(parameters);
    if validator.check(&args) {
        return Ok(args);
    }
    let errors: Vec<JsonString> = validator
        .errors(&args)
        .iter()
        .map(|error| {
            concat([
                &key("  - "),
                &format_validation_path(error),
                &key(": "),
                &error.message,
            ])
        })
        .collect();
    let errors = if errors.is_empty() {
        key("Unknown validation error")
    } else {
        join(&errors, "\n")
    };
    let received = key(&stringify_pretty(arguments, "  "));
    Err(concat([
        &key("Validation failed for tool \""),
        name,
        &key("\":\n"),
        &errors,
        &key("\n\nReceived arguments:\n"),
        &received,
    ]))
}
