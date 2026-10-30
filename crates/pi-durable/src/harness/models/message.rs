//! Provider values can remain hosted until a durable writer requests strict JSON.
use crate::*;
use parking_lot::Mutex;
use std::{any::Any, sync::Arc};

pub trait HostedAssistantValue: Any + Send + Sync {
    fn field(&self, name: &str) -> Result<Option<JsonValue>>;
    fn set_field(&self, name: JsonString, value: Option<JsonValue>) -> Result<()>;
    fn copy_json(&self) -> Result<JsonObject>;
    fn has_content(&self) -> Result<bool> {
        Ok(!content(self.field("content")?)?.is_empty())
    }
    fn tool_calls(&self) -> Result<Vec<JsonValue>> {
        Ok(tool_calls(content(self.field("content")?)?))
    }
    fn has_tool_calls(&self) -> Result<bool> {
        Ok(!self.tool_calls()?.is_empty())
    }
    fn text_blocks(&self) -> Result<Vec<JsonString>> {
        text_blocks(content(self.field("content")?)?)
    }
}
fn content(value: Option<JsonValue>) -> Result<Vec<JsonValue>> {
    wire::decode(&value.ok_or_else(|| Error::message("Assistant message has no content"))?)
        .map_err(Error::message)
}
fn tool_calls(content: Vec<JsonValue>) -> Vec<JsonValue> {
    content
        .into_iter()
        .filter(|block| block["type"].string_equals("toolCall"))
        .collect()
}
fn text_blocks(content: Vec<JsonValue>) -> Result<Vec<JsonString>> {
    content
        .iter()
        .filter(|block| block["type"].string_equals("text"))
        .map(|block| wire::field(block, "text").map_err(Error::message))
        .collect()
}
enum Value {
    Json(Mutex<JsonObject>),
    Hosted(Arc<dyn HostedAssistantValue>),
}
pub struct AssistantValue {
    value: Value,
}
pub type AssistantValueRef = Arc<AssistantValue>;
impl AssistantValue {
    pub fn json(value: JsonObject) -> AssistantValueRef {
        Arc::new(Self {
            value: Value::Json(Mutex::new(value)),
        })
    }
    pub fn hosted(value: Arc<dyn HostedAssistantValue>) -> AssistantValueRef {
        Arc::new(Self {
            value: Value::Hosted(value),
        })
    }
    pub fn host<T: Any>(&self) -> Option<&T> {
        match &self.value {
            Value::Hosted(value) => {
                let value: &dyn Any = value.as_ref();
                value.downcast_ref()
            }
            Value::Json(_) => None,
        }
    }
    pub fn field(&self, name: &str) -> Result<Option<JsonValue>> {
        match &self.value {
            Value::Json(value) => Ok(value.lock().get(&JsonString::from(name)).cloned()),
            Value::Hosted(value) => value.field(name),
        }
    }
    pub fn required<T: serde::de::DeserializeOwned>(&self, name: &str) -> Result<T> {
        let value = self
            .field(name)?
            .ok_or_else(|| Error::message(format!("Assistant message has no {name}")))?;
        wire::decode(&value).map_err(Error::message)
    }
    pub fn set_field(&self, name: impl Into<JsonString>, value: Option<JsonValue>) -> Result<()> {
        let name = name.into();
        match &self.value {
            Value::Json(object) => {
                let mut object = object.lock();
                if let Some(value) = value {
                    object.insert(name, value);
                } else {
                    object.shift_remove(&name);
                }
                Ok(())
            }
            Value::Hosted(object) => object.set_field(name, value),
        }
    }
    pub fn copy_json(&self) -> Result<JsonObject> {
        match &self.value {
            Value::Json(value) => Ok(value.lock().clone()),
            Value::Hosted(value) => value.copy_json(),
        }
    }
    pub fn has_content(&self) -> Result<bool> {
        match &self.value {
            Value::Json(_) => Ok(!self.required::<Vec<JsonValue>>("content")?.is_empty()),
            Value::Hosted(value) => value.has_content(),
        }
    }
    pub fn tool_calls(&self) -> Result<Vec<JsonValue>> {
        match &self.value {
            Value::Json(_) => Ok(tool_calls(self.required("content")?)),
            Value::Hosted(value) => value.tool_calls(),
        }
    }
    pub fn has_tool_calls(&self) -> Result<bool> {
        match &self.value {
            Value::Json(_) => Ok(!self.tool_calls()?.is_empty()),
            Value::Hosted(value) => value.has_tool_calls(),
        }
    }
    pub fn text_blocks(&self) -> Result<Vec<JsonString>> {
        match &self.value {
            Value::Json(_) => text_blocks(self.required("content")?),
            Value::Hosted(value) => value.text_blocks(),
        }
    }
}
impl std::fmt::Debug for AssistantValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.value {
            Value::Json(value) => value.lock().fmt(f),
            Value::Hosted(_) => f.write_str("HostedAssistantValue"),
        }
    }
}
