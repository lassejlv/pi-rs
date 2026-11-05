//! Decode tagged records from raw JSON rather than serde's buffered enum content.
//! This preserves UTF-16 strings inside arbitrary nested payloads.

use pi_chord::{JsonString, JsonValue};
use serde::{Serialize, de::DeserializeOwned};

pub fn get<'a>(value: &'a JsonValue, key: &str) -> Option<&'a JsonValue> {
    value.as_object()?.get(&JsonString::from(key))
}
pub fn field<T: DeserializeOwned>(value: &JsonValue, key: &str) -> std::result::Result<T, String> {
    decode(get(value, key).ok_or_else(|| format!("Missing field {key}"))?)
}
pub fn optional<T: DeserializeOwned>(
    value: &JsonValue,
    key: &str,
) -> std::result::Result<Option<T>, String> {
    get(value, key).map(decode).transpose()
}
pub fn decode<T: DeserializeOwned>(value: &JsonValue) -> std::result::Result<T, String> {
    pi_chord::json::from_value(value)
}
pub fn tag(value: &JsonValue, key: &str) -> std::result::Result<String, String> {
    get(value, key)
        .and_then(JsonValue::to_utf8)
        .ok_or_else(|| format!("Missing string discriminator {key}"))
}
pub fn encode<T: Serialize + ?Sized>(value: &T) -> JsonValue {
    pi_chord::json::to_value(value).expect("durable record fields are JSON serializable")
}
pub fn object() -> JsonValue {
    JsonValue::Object(Default::default())
}
pub fn put<T: Serialize + ?Sized>(value: &mut JsonValue, key: &str, field: &T) {
    value
        .as_object_mut()
        .expect("wire record object")
        .insert(key.into(), encode(field));
}
pub fn put_optional<T: Serialize>(value: &mut JsonValue, key: &str, field: &Option<T>) {
    if let Some(field) = field {
        put(value, key, field);
    }
}

macro_rules! raw_serde {
    ($name:ty) => {
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                self.to_json().serialize(serializer)
            }
        }
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(
                deserializer: D,
            ) -> std::result::Result<Self, D::Error> {
                let value = pi_chord::JsonValue::deserialize(deserializer)?;
                Self::from_json(&value).map_err(serde::de::Error::custom)
            }
        }
    };
}
pub(crate) use raw_serde;
