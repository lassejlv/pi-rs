use crate::{JsonValue, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::Error};
use std::{any::Any, sync::Arc};

/// An extension value that must remain available to hooks before the durable
/// writer copies it to strict JSON. A host can retain functions or cycles here;
/// `copy_json` rejects them at the same boundary as ordinary durable writes.
pub trait HostedToolValue: Any + Send + Sync {
    fn copy_json(&self) -> Result<JsonValue>;
}
#[derive(Clone)]
pub enum ToolValue {
    Json(JsonValue),
    Hosted(Arc<dyn HostedToolValue>),
}
impl ToolValue {
    pub fn copy_json(&self) -> Result<JsonValue> {
        match self {
            Self::Json(value) => Ok(value.clone()),
            Self::Hosted(value) => value.copy_json(),
        }
    }
    pub fn hosted<T: Any>(&self) -> Option<&T> {
        match self {
            Self::Hosted(value) => {
                let value: &dyn Any = value.as_ref();
                value.downcast_ref()
            }
            Self::Json(_) => None,
        }
    }
}
impl From<JsonValue> for ToolValue {
    fn from(value: JsonValue) -> Self {
        Self::Json(value)
    }
}
impl std::fmt::Debug for ToolValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(value) => value.fmt(f),
            Self::Hosted(_) => f.write_str("HostedToolValue"),
        }
    }
}
impl Serialize for ToolValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.copy_json()
            .map_err(S::Error::custom)?
            .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for ToolValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        JsonValue::deserialize(deserializer).map(Self::Json)
    }
}
