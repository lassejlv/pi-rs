//! Strict JSON retaining all JavaScript UTF-16 strings, including lone surrogates.
//!
//! `serde_json::Value` cannot represent these strings. Persisted Chord state can:
//! truncating one UTF-16 code unit can split a surrogate pair.

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, ops::Index, sync::Arc};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct JsonString(Vec<u16>);

impl JsonString {
    pub fn from_utf16(units: Vec<u16>) -> Self {
        Self(units)
    }
    pub fn units(&self) -> &[u16] {
        &self.0
    }
    pub fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(&self.0)
    }
    pub fn to_string_checked(&self) -> Result<String, std::string::FromUtf16Error> {
        String::from_utf16(&self.0)
    }
    pub fn append(&mut self, suffix: &Self) {
        self.0.extend_from_slice(&suffix.0);
    }
    pub fn truncate_front(&mut self, count: usize) {
        self.0.drain(..count.min(self.0.len()));
    }
    pub fn equals_str(&self, text: &str) -> bool {
        self.0.iter().copied().eq(text.encode_utf16())
    }
}

impl From<&str> for JsonString {
    fn from(value: &str) -> Self {
        Self(value.encode_utf16().collect())
    }
}
impl From<String> for JsonString {
    fn from(value: String) -> Self {
        Self::from(value.as_str())
    }
}
impl fmt::Display for JsonString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

pub type Map = IndexMap<JsonString, Value>;

#[derive(Clone, Debug, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(JsonString),
    Array(Arc<Vec<Value>>),
    Object(Arc<Map>),
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Number(a), Self::Number(b)) => a.as_f64() == b.as_f64(),
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Array(a), Self::Array(b)) => Arc::ptr_eq(a, b) || a == b,
            (Self::Object(a), Self::Object(b)) => Arc::ptr_eq(a, b) || a == b,
            _ => false,
        }
    }
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
    pub fn is_number(&self) -> bool {
        matches!(self, Self::Number(_))
    }
    pub fn is_string(&self) -> bool {
        matches!(self, Self::String(_))
    }
    pub fn is_array(&self) -> bool {
        matches!(self, Self::Array(_))
    }
    pub fn is_object(&self) -> bool {
        matches!(self, Self::Object(_))
    }
    pub fn as_f64(&self) -> Option<f64> {
        if let Self::Number(n) = self {
            n.as_f64()
        } else {
            None
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        if let Self::Number(n) = self {
            n.as_u64()
        } else {
            None
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let Self::Bool(b) = self {
            Some(*b)
        } else {
            None
        }
    }
    pub fn as_string(&self) -> Option<&JsonString> {
        if let Self::String(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub fn as_array(&self) -> Option<&Vec<Value>> {
        if let Self::Array(a) = self {
            Some(a)
        } else {
            None
        }
    }
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        if let Self::Array(a) = self {
            Some(Arc::make_mut(a))
        } else {
            None
        }
    }
    pub fn as_object(&self) -> Option<&Map> {
        if let Self::Object(m) = self {
            Some(m)
        } else {
            None
        }
    }
    pub fn as_object_mut(&mut self) -> Option<&mut Map> {
        if let Self::Object(m) = self {
            Some(Arc::make_mut(m))
        } else {
            None
        }
    }
    /// Compare a protocol discriminant without allocating a UTF-8 conversion.
    pub fn string_equals(&self, text: &str) -> bool {
        self.as_string().is_some_and(|s| s.equals_str(text))
    }
    pub fn to_utf8(&self) -> Option<String> {
        self.as_string()?.to_string_checked().ok()
    }
}

impl Index<&str> for Value {
    type Output = Value;
    fn index(&self, key: &str) -> &Value {
        static NULL: Value = Value::Null;
        self.as_object()
            .and_then(|m| m.get(&JsonString::from(key)))
            .unwrap_or(&NULL)
    }
}
impl Index<usize> for Value {
    type Output = Value;
    fn index(&self, key: usize) -> &Value {
        static NULL: Value = Value::Null;
        self.as_array().and_then(|a| a.get(key)).unwrap_or(&NULL)
    }
}
impl From<serde_json::Value> for Value {
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(b) => Self::Bool(b),
            serde_json::Value::Number(n) => Self::Number(n),
            serde_json::Value::String(s) => Self::String(s.into()),
            serde_json::Value::Array(a) => {
                Self::Array(a.into_iter().map(Self::from).collect::<Vec<_>>().into())
            }
            serde_json::Value::Object(m) => Self::Object(
                m.into_iter()
                    .map(|(k, v)| (k.into(), v.into()))
                    .collect::<Map>()
                    .into(),
            ),
        }
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Self::String(s.into())
    }
}
impl From<usize> for Value {
    fn from(n: usize) -> Self {
        Self::Number(n.into())
    }
}

fn quote(value: &JsonString, out: &mut String) {
    out.push('"');
    let units = &value.0;
    out.reserve(units.len());
    let mut at = 0;
    while at < units.len() {
        // A run of printable ASCII needs no escape and no UTF-16 decoding.
        let start = at;
        while at < units.len()
            && matches!(units[at], 0x20..=0x7e)
            && units[at] != 0x22
            && units[at] != 0x5c
        {
            at += 1;
        }
        out.extend(units[start..at].iter().map(|unit| *unit as u8 as char));
        if at == units.len() {
            break;
        }
        let window = &units[at..units.len().min(at + 2)];
        let Some(character) = char::decode_utf16(window.iter().copied()).next() else {
            break;
        };
        at += match character {
            Ok(c) => c.len_utf16(),
            Err(_) => 1,
        };
        match character {
            Ok('"') => out.push_str("\\\""),
            Ok('\\') => out.push_str("\\\\"),
            Ok('\u{8}') => out.push_str("\\b"),
            Ok('\u{c}') => out.push_str("\\f"),
            Ok('\n') => out.push_str("\\n"),
            Ok('\r') => out.push_str("\\r"),
            Ok('\t') => out.push_str("\\t"),
            Ok(c) if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            Ok(c) => out.push(c),
            Err(e) => out.push_str(&format!("\\u{:04x}", e.unpaired_surrogate())),
        }
    }
    out.push('"');
}

/// ECMAScript own-property order: array-index keys, then other keys in insertion order.
pub fn object_entries(map: &Map) -> impl Iterator<Item = (&JsonString, &Value)> {
    let mut indices = Vec::new();
    let mut other = Vec::new();
    for (key, value) in map {
        let index = key.to_string_checked().ok().and_then(|s| {
            s.parse::<u32>()
                .ok()
                .filter(|n| *n < u32::MAX && n.to_string() == s)
        });
        if let Some(index) = index {
            indices.push((index, key, value));
        } else {
            other.push((key, value));
        }
    }
    indices.sort_by_key(|(index, _, _)| *index);
    indices
        .into_iter()
        .map(|(_, key, value)| (key, value))
        .chain(other)
}
pub fn stringify(value: &Value) -> String {
    stringify_pretty(value, "")
}
/// `JSON.stringify(value, null, indent)`: ECMAScript key order and `\uXXXX`
/// escapes for unpaired UTF-16 units.
pub fn stringify_pretty(value: &Value, indent: &str) -> String {
    fn newline(out: &mut String, indent: &str, depth: usize) {
        if !indent.is_empty() {
            out.push('\n');
            for _ in 0..depth {
                out.push_str(indent);
            }
        }
    }
    fn write(value: &Value, indent: &str, depth: usize, out: &mut String) {
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&crate::number_to_string(
                n.as_f64().expect("finite JSON number"),
            )),
            Value::String(s) => quote(s, out),
            Value::Array(a) if a.is_empty() => out.push_str("[]"),
            Value::Array(a) => {
                out.push('[');
                for (index, item) in a.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    newline(out, indent, depth + 1);
                    write(item, indent, depth + 1, out);
                }
                newline(out, indent, depth);
                out.push(']');
            }
            Value::Object(m) if m.is_empty() => out.push_str("{}"),
            Value::Object(m) => {
                out.push('{');
                for (i, (key, value)) in object_entries(m).enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, indent, depth + 1);
                    quote(key, out);
                    out.push(':');
                    if !indent.is_empty() {
                        out.push(' ');
                    }
                    write(value, indent, depth + 1, out);
                }
                newline(out, indent, depth);
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    write(value, indent, 0, &mut out);
    out
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&stringify(self))
    }
}

/// The text form of a value, written as raw JSON so every serde serializer can carry it unchanged.
struct RawText<'a>(&'a Value);
impl Serialize for RawText<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde_json::value::RawValue::from_string(stringify(self.0))
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}
impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // This crate's own serializer takes the value itself (lossless, no text); any other serializer
        // ignores the newtype name and receives the raw JSON text.
        crate::direct::offer_for_serialize(self);
        let result = serializer.serialize_newtype_struct(crate::direct::MARKER, &RawText(self));
        crate::direct::withdraw_serialize_offer();
        result
    }
}
impl Serialize for JsonString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Value::String(self.clone()).serialize(serializer)
    }
}
struct ValueVisitor;
impl<'de> serde::de::Visitor<'de> for ValueVisitor {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Value, D::Error> {
        // This crate's own deserializer hands the value over directly; any other deserializer yields raw JSON.
        if let Some(value) = crate::direct::take_deserialize_offer() {
            return Ok(value);
        }
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        parse(raw.get()).map_err(serde::de::Error::custom)
    }
}
impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_newtype_struct(crate::direct::MARKER, ValueVisitor)
    }
}
impl<'de> Deserialize<'de> for JsonString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(s) => Ok(s),
            _ => Err(serde::de::Error::custom("expected a JSON string")),
        }
    }
}

/// Serialize into a `Value`. The direct serializer keeps raw UTF-16 strings; a value it cannot represent falls back
/// to the JSON text route, which reports the error serde_json always reported.
pub fn to_value<T: Serialize + ?Sized>(value: &T) -> Result<Value, String> {
    match crate::direct::to_value_direct(value) {
        Ok(value) => Ok(value),
        Err(_) => parse(&serde_json::to_string(value).map_err(|e| e.to_string())?),
    }
}
/// Deserialize from a `Value` without JSON text, falling back to the text route (and its error messages) when the
/// direct deserializer cannot read the value.
pub fn from_value<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, String> {
    match crate::direct::from_value_direct(value) {
        Ok(value) => Ok(value),
        Err(_) => serde_json::from_str(&stringify(value)).map_err(|e| e.to_string()),
    }
}

pub fn parse(input: &str) -> Result<Value, String> {
    struct Parser<'a> {
        input: &'a str,
        at: usize,
        depth: usize,
    }
    impl Parser<'_> {
        fn ws(&mut self) {
            while self
                .input
                .as_bytes()
                .get(self.at)
                .is_some_and(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
            {
                self.at += 1;
            }
        }
        fn byte(&self) -> Option<u8> {
            self.input.as_bytes().get(self.at).copied()
        }
        fn error(&self) -> String {
            format!("Invalid JSON at byte {}", self.at)
        }
        fn string(&mut self) -> Result<JsonString, String> {
            if self.byte() != Some(b'"') {
                return Err(self.error());
            }
            self.at += 1;
            let mut units = Vec::new();
            loop {
                match self.byte() {
                    Some(b'"') => {
                        self.at += 1;
                        return Ok(JsonString(units));
                    }
                    Some(b'\\') => {
                        self.at += 1;
                        let b = self.byte().ok_or_else(|| self.error())?;
                        self.at += 1;
                        units.push(match b {
                            b'"' => 34,
                            b'\\' => 92,
                            b'/' => 47,
                            b'b' => 8,
                            b'f' => 12,
                            b'n' => 10,
                            b'r' => 13,
                            b't' => 9,
                            b'u' => {
                                let end = self.at + 4;
                                let hex =
                                    self.input.get(self.at..end).ok_or_else(|| self.error())?;
                                if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                                    return Err(self.error());
                                }
                                let unit =
                                    u16::from_str_radix(hex, 16).map_err(|_| self.error())?;
                                self.at = end;
                                unit
                            }
                            _ => return Err(self.error()),
                        });
                    }
                    Some(b) if b < 32 => return Err(self.error()),
                    Some(b) if b < 0x80 => {
                        // A run of plain ASCII needs no per-character decoding.
                        let bytes = self.input.as_bytes();
                        let start = self.at;
                        let mut end = start + 1;
                        while end < bytes.len()
                            && (0x20..0x80).contains(&bytes[end])
                            && bytes[end] != b'"'
                            && bytes[end] != b'\\'
                        {
                            end += 1;
                        }
                        units.extend(bytes[start..end].iter().map(|byte| u16::from(*byte)));
                        self.at = end;
                    }
                    Some(_) => {
                        let c = self.input[self.at..]
                            .chars()
                            .next()
                            .ok_or_else(|| self.error())?;
                        self.at += c.len_utf8();
                        units.extend_from_slice(c.encode_utf16(&mut [0; 2]));
                    }
                    None => return Err(self.error()),
                }
            }
        }
        fn value(&mut self) -> Result<Value, String> {
            self.ws();
            if self.depth >= 128 {
                return Err("JSON nesting exceeds 128".into());
            }
            match self.byte() {
                Some(b'"') => self.string().map(Value::String),
                Some(b'[') => {
                    self.at += 1;
                    self.depth += 1;
                    self.ws();
                    let mut a = Vec::new();
                    if self.byte() != Some(b']') {
                        loop {
                            a.push(self.value()?);
                            self.ws();
                            if self.byte() != Some(b',') {
                                break;
                            }
                            self.at += 1;
                        }
                    }
                    if self.byte() != Some(b']') {
                        return Err(self.error());
                    }
                    self.at += 1;
                    self.depth -= 1;
                    Ok(Value::Array(a.into()))
                }
                Some(b'{') => {
                    self.at += 1;
                    self.depth += 1;
                    self.ws();
                    let mut m = Map::new();
                    if self.byte() != Some(b'}') {
                        loop {
                            self.ws();
                            let key = self.string()?;
                            self.ws();
                            if self.byte() != Some(b':') {
                                return Err(self.error());
                            }
                            self.at += 1;
                            m.insert(key, self.value()?);
                            self.ws();
                            if self.byte() != Some(b',') {
                                break;
                            }
                            self.at += 1;
                        }
                    }
                    if self.byte() != Some(b'}') {
                        return Err(self.error());
                    }
                    self.at += 1;
                    self.depth -= 1;
                    Ok(Value::Object(m.into()))
                }
                Some(b'n') if self.input[self.at..].starts_with("null") => {
                    self.at += 4;
                    Ok(Value::Null)
                }
                Some(b't') if self.input[self.at..].starts_with("true") => {
                    self.at += 4;
                    Ok(Value::Bool(true))
                }
                Some(b'f') if self.input[self.at..].starts_with("false") => {
                    self.at += 5;
                    Ok(Value::Bool(false))
                }
                Some(b'-' | b'0'..=b'9') => {
                    let start = self.at;
                    while self.byte().is_some_and(|b| {
                        matches!(b, b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                    }) {
                        self.at += 1;
                    }
                    serde_json::from_str::<serde_json::Number>(&self.input[start..self.at])
                        .map(Value::Number)
                        .map_err(|e| e.to_string())
                }
                _ => Err(self.error()),
            }
        }
    }
    let mut parser = Parser {
        input,
        at: 0,
        depth: 0,
    };
    let result = parser.value()?;
    parser.ws();
    if parser.at != input.len() {
        return Err(parser.error());
    }
    Ok(result)
}
