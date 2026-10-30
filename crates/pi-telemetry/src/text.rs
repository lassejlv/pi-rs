//! Telemetry strings retain JavaScript code units and a borrowed UTF-8 view when available.
use pi_json::strict::{JsonString, Value};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Immutable text accepted by telemetry adapters, including unpaired UTF-16 surrogates.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TelemetryText {
    units: JsonString,
    utf8: Option<String>,
}
impl TelemetryText {
    pub fn as_str(&self) -> Option<&str> {
        self.utf8.as_deref()
    }
    pub fn units(&self) -> &[u16] {
        self.units.units()
    }
    pub fn as_json_string(&self) -> &JsonString {
        &self.units
    }
    pub fn into_json_string(self) -> JsonString {
        self.units
    }
    /// JavaScript `slice(0, count)`, which may split a surrogate pair.
    pub fn prefix(&self, count: usize) -> Self {
        JsonString::from_utf16(self.units()[..count.min(self.units().len())].to_vec()).into()
    }
}
impl From<String> for TelemetryText {
    fn from(value: String) -> Self {
        Self {
            units: value.as_str().into(),
            utf8: Some(value),
        }
    }
}
impl From<&str> for TelemetryText {
    fn from(value: &str) -> Self {
        value.to_owned().into()
    }
}
impl From<&String> for TelemetryText {
    fn from(value: &String) -> Self {
        value.as_str().into()
    }
}
impl From<JsonString> for TelemetryText {
    fn from(units: JsonString) -> Self {
        let utf8 = units.to_string_checked().ok();
        Self { units, utf8 }
    }
}
impl From<&JsonString> for TelemetryText {
    fn from(value: &JsonString) -> Self {
        value.clone().into()
    }
}
impl std::fmt::Display for TelemetryText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.utf8 {
            Some(value) => f.write_str(value),
            None => std::fmt::Display::fmt(&self.units, f),
        }
    }
}
impl Serialize for TelemetryText {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.units.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for TelemetryText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        JsonString::deserialize(deserializer).map(Into::into)
    }
}
impl From<TelemetryText> for Value {
    fn from(value: TelemetryText) -> Self {
        Self::String(value.into_json_string())
    }
}

/// JavaScript string comparison: UTF-16 code unit order.
impl PartialOrd for TelemetryText {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TelemetryText {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.units().cmp(other.units())
    }
}
impl PartialEq<str> for TelemetryText {
    fn eq(&self, other: &str) -> bool {
        self.units.equals_str(other)
    }
}
impl PartialEq<&str> for TelemetryText {
    fn eq(&self, other: &&str) -> bool {
        self.units.equals_str(other)
    }
}
