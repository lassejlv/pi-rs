//! Model-visible declarations stay lossless until a provider adapter converts them.
use crate::{wire, *};
use indexmap::IndexMap;

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDeclaration {
    pub name: JsonString,
    pub description: JsonString,
    pub parameters: JsonValue,
    pub constrained_sampling: Option<JsonValue>,
}
impl ToolDeclaration {
    pub fn new(
        name: impl Into<JsonString>,
        description: impl Into<JsonString>,
        parameters: JsonValue,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            constrained_sampling: None,
        }
    }
    pub fn from_tool(value: &JsonObject) -> Result<Self> {
        Self::from_json(value.value()).map_err(Error::message)
    }
    pub fn same_declaration(&self, other: &Self) -> bool {
        self.to_json().to_string() == other.to_json().to_string()
    }
    fn to_json(&self) -> JsonValue {
        let mut value = wire::object();
        wire::put(&mut value, "name", &self.name);
        wire::put(&mut value, "description", &self.description);
        wire::put(&mut value, "parameters", &self.parameters);
        wire::put_optional(
            &mut value,
            "constrainedSampling",
            &self.constrained_sampling,
        );
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        Ok(Self {
            name: wire::field(value, "name")?,
            description: wire::field(value, "description")?,
            parameters: wire::field(value, "parameters")?,
            constrained_sampling: wire::optional(value, "constrainedSampling")?,
        })
    }
}
wire::raw_serde!(ToolDeclaration);

/// Replay model-visible tool state without coercing names or JSON Schema strings to UTF8.
pub fn current_tool_declarations(
    messages: &[std::sync::Arc<JsonValue>],
) -> Result<Vec<ToolDeclaration>> {
    let mut tools = IndexMap::new();
    for message in messages {
        if !message["role"].string_equals("system") {
            continue;
        }
        if let Some(removed) = wire::get(message, "toolsRemoved") {
            for tool in removed
                .as_array()
                .ok_or_else(|| Error::message("Expected tool removals"))?
            {
                tools.shift_remove(
                    &wire::field::<JsonString>(tool, "name").map_err(Error::message)?,
                );
            }
        }
        if let Some(added) = wire::get(message, "toolsAdded") {
            for tool in added
                .as_array()
                .ok_or_else(|| Error::message("Expected tool additions"))?
            {
                let tool: ToolDeclaration = wire::decode(tool).map_err(Error::message)?;
                tools.insert(tool.name.clone(), tool);
            }
        }
    }
    Ok(tools.into_values().collect())
}
/// Registration metadata and executable callbacks are separate from persisted declarations.
pub trait DeclaredTool: super::registry::RegisteredTool {
    fn declaration(&self) -> ToolDeclaration;
}
