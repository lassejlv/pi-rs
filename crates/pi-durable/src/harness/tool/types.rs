use super::ToolValue;
pub use crate::harness::live::{DiagnosticSeverity, ToolDiagnostic};
use crate::{wire, *};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock};

/// Both array and block identities survive an afterTool hook that retains them.
pub type ToolContent = Arc<Vec<Arc<JsonObject>>>;

// Raw JSON dispatch avoids serde's buffered tagged/flattened representations,
// which cannot represent lone UTF16 units inside arbitrary result payloads.
macro_rules! object_record {
    ($(#[$attr:meta])* $name:ident {$($mode:ident $field:ident : $ty:ty => $key:literal),* $(,)?}) => {
        #[derive(Clone, Debug)] $(#[$attr])*
        pub struct $name { $(pub $field: object_record!(@type $mode $ty)),* }
        impl $name {
            fn to_json(&self) -> JsonValue {
                let mut value = wire::object();
                $(object_record!(@put $mode &mut value, $key, &self.$field);)* value
            }
            fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
                Ok(Self { $($field: object_record!(@get $mode value, $key)?),* })
            }
        }
        wire::raw_serde!($name);
    };
    (@type required $ty:ty) => {$ty};
    (@type optional $ty:ty) => {Option<$ty>};
    (@put required $value:expr, $key:expr, $field:expr) => {wire::put($value, $key, $field)};
    (@put optional $value:expr, $key:expr, $field:expr) => {wire::put_optional($value, $key, $field)};
    (@get required $value:expr, $key:expr) => {wire::field($value, $key)};
    (@get optional $value:expr, $key:expr) => {wire::optional($value, $key)};
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: JsonString,
    pub name: JsonString,
    pub arguments: JsonValue,
    pub extra: JsonObject,
}
impl ToolCall {
    pub fn new(
        id: impl Into<JsonString>,
        name: impl Into<JsonString>,
        arguments: JsonValue,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
            extra: JsonObject::default(),
        }
    }
    pub(crate) fn to_json(&self) -> JsonValue {
        let mut value = self.extra.clone().into_value();
        wire::put(&mut value, "type", &"toolCall");
        wire::put(&mut value, "id", &self.id);
        wire::put(&mut value, "name", &self.name);
        wire::put(&mut value, "arguments", &self.arguments);
        value
    }
    pub(crate) fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        let mut extra = wire::decode::<JsonObject>(value)?;
        for key in ["type", "id", "name", "arguments"] {
            extra.shift_remove(&JsonString::from(key));
        }
        Ok(Self {
            id: wire::field(value, "id")?,
            name: wire::field(value, "name")?,
            arguments: wire::field(value, "arguments")?,
            extra,
        })
    }
}
wire::raw_serde!(ToolCall);
object_record!(#[derive(Default)] ToolControl {
    optional add_tools: Vec<JsonString> => "addTools",
    optional terminate: bool => "terminate",
    optional handoff: JsonString => "handoff",
});
object_record!(#[derive(Default)] ToolExecutionResult {
    optional output: ToolContent => "output",
    optional structured_output: ToolValue => "structuredOutput",
    optional is_error: bool => "isError",
    optional details: ToolValue => "details",
    optional diagnostics: Vec<ToolDiagnostic> => "diagnostics",
    optional usage: JsonObject => "usage",
    optional control: ToolControl => "control",
});
object_record!(NestedToolExecutionResult {
    required task_id: TaskId => "taskId",
    optional structured_output: JsonValue => "structuredOutput",
    required is_error: bool => "isError",
    optional details: JsonValue => "details",
    required diagnostics: Vec<ToolDiagnostic> => "diagnostics",
    optional usage: JsonObject => "usage",
    optional duration_ms: f64 => "durationMs",
});
#[derive(Clone, Debug)]
pub enum ToolTaskInput {
    Model {
        assistant: EntryId,
        call_id: JsonString,
    },
    Nested {
        parent: TaskId,
        parent_call_id: JsonString,
        key: JsonString,
        call: Box<ToolCall>,
        progress: Option<bool>,
    },
}
impl ToolTaskInput {
    pub fn model(assistant: EntryId, call_id: impl Into<JsonString>) -> Self {
        Self::Model {
            assistant,
            call_id: call_id.into(),
        }
    }
    pub fn is_nested(&self) -> bool {
        matches!(self, Self::Nested { .. })
    }
    fn to_json(&self) -> JsonValue {
        let mut value = wire::object();
        match self {
            Self::Model { assistant, call_id } => {
                wire::put(&mut value, "kind", &"model");
                wire::put(&mut value, "assistant", assistant);
                wire::put(&mut value, "callId", call_id);
            }
            Self::Nested {
                parent,
                parent_call_id,
                key,
                call,
                progress,
            } => {
                wire::put(&mut value, "kind", &"nested");
                wire::put(&mut value, "parent", parent);
                wire::put(&mut value, "parentCallId", parent_call_id);
                wire::put(&mut value, "key", key);
                wire::put(&mut value, "call", call);
                wire::put_optional(&mut value, "progress", progress);
            }
        }
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        Ok(match wire::tag(value, "kind")?.as_str() {
            "model" => Self::Model {
                assistant: wire::field(value, "assistant")?,
                call_id: wire::field(value, "callId")?,
            },
            "nested" => Self::Nested {
                parent: wire::field(value, "parent")?,
                parent_call_id: wire::field(value, "parentCallId")?,
                key: wire::field(value, "key")?,
                call: wire::field(value, "call")?,
                progress: wire::optional(value, "progress")?,
            },
            kind => return Err(format!("Unknown tool task input kind {kind}")),
        })
    }
}
wire::raw_serde!(ToolTaskInput);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplayPolicy {
    Safe,
    Unsafe,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionMode {
    Parallel,
    Sequential,
}
#[derive(Clone, Debug)]
pub enum ToolTaskCheckpoint {
    Call,
    Execute {
        arguments: JsonObject,
        replay: ReplayPolicy,
    },
}
impl ToolTaskCheckpoint {
    fn to_json(&self) -> JsonValue {
        let mut value = wire::object();
        match self {
            Self::Call => wire::put(&mut value, "phase", &"call"),
            Self::Execute { arguments, replay } => {
                wire::put(&mut value, "phase", &"execute");
                wire::put(&mut value, "arguments", arguments);
                wire::put(&mut value, "replay", replay);
            }
        }
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        Ok(match wire::tag(value, "phase")?.as_str() {
            "call" => Self::Call,
            "execute" => Self::Execute {
                arguments: wire::field(value, "arguments")?,
                replay: wire::field(value, "replay")?,
            },
            phase => return Err(format!("Unknown tool task phase {phase}")),
        })
    }
}
wire::raw_serde!(ToolTaskCheckpoint);
#[derive(Clone, Debug)]
pub enum ToolTaskResult {
    Model {
        entry_id: EntryId,
        control: Option<ToolControl>,
    },
    /// A version-1 task already held this receipt at upgrade time.
    LegacyModel {
        entry_id: EntryId,
        control: Option<ToolControl>,
    },
    Nested,
}
impl ToolTaskResult {
    fn to_json(&self) -> JsonValue {
        let mut value = wire::object();
        match self {
            Self::Model { entry_id, control } | Self::LegacyModel { entry_id, control } => {
                if matches!(self, Self::Model { .. }) {
                    wire::put(&mut value, "kind", &"model");
                }
                wire::put(&mut value, "entryId", entry_id);
                wire::put_optional(&mut value, "control", control);
            }
            Self::Nested => wire::put(&mut value, "kind", &"nested"),
        }
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        Ok(
            match wire::get(value, "kind")
                .and_then(JsonValue::to_utf8)
                .as_deref()
            {
                Some("nested") => Self::Nested,
                Some("model") => Self::Model {
                    entry_id: wire::field(value, "entryId")?,
                    control: wire::optional(value, "control")?,
                },
                None if wire::get(value, "kind").is_none() => Self::LegacyModel {
                    entry_id: wire::field(value, "entryId")?,
                    control: wire::optional(value, "control")?,
                },
                kind => return Err(format!("Unknown tool task result kind {kind:?}")),
            },
        )
    }
}
wire::raw_serde!(ToolTaskResult);
object_record!(#[derive(Default)] NestedCallState {
    optional task_id: TypedTaskId<ToolTaskResult> => "taskId",
    optional result: NestedToolExecutionResult => "result",
});
impl DocumentState for NestedCallState {}
pub static NESTED_CALL_DOC: LazyLock<TaskDocFamilyToken<NestedCallState, ()>> =
    LazyLock::new(|| {
        define_doc_family(DocFamilyDefinition::new(
            "pi.tool.nested-call",
            1,
            TaskScope,
            |()| Ok(NestedCallState::default()),
            |_, _, _| false,
        ))
        .expect("valid built-in nested-call family")
    });
