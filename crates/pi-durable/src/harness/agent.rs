//! Stored conversation choices and resolution against one immutable registry.
use super::{
    reader::RegistryView,
    registry::*,
    settings::{Settings, SettingsReader, read_settings},
};
use crate::{storage::StorageFuture, wire, *};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use std::{
    any::Any,
    sync::{Arc, LazyLock},
};

pub const INSTRUCTIONS_KEY: &str = "instructions";
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentState(pub JsonObject);
impl DocumentState for AgentState {}
pub static AGENT_DOC: LazyLock<RewindableConversationDocToken<AgentState>> = LazyLock::new(|| {
    define_doc(DocDefinition::new(
        "pi.agent",
        1,
        RewindableConversationScope {
            fork: ForkPolicy::AsOf,
        },
        || Ok(AgentState::default()),
        |_, _, _| true,
    ))
    .unwrap()
});

/// Wire change with named tool/extension objects. Unknown stored fields survive
/// configuration; null clears a field and an omitted field leaves it alone.
pub fn apply_change(state: &mut AgentState, change: &JsonObject) -> Result<()> {
    for key in [
        "model",
        "thinkingLevel",
        "extensions",
        "tools",
        "modelTools",
        "instructions",
        "cwd",
    ] {
        let key = JsonString::from(key);
        let Some(value) = change.get(&key) else {
            continue;
        };
        if value.is_null() {
            state.0.shift_remove(&key);
            continue;
        }
        let value = if ["extensions", "tools", "modelTools"]
            .iter()
            .any(|name| key == JsonString::from(*name))
        {
            fn names(value: &JsonValue) -> Result<JsonValue> {
                let items = value
                    .as_array()
                    .ok_or_else(|| Error::message("Expected named choices"))?;
                Ok(JsonValue::Array(
                    items
                        .iter()
                        .map(|item| {
                            wire::get(item, "name")
                                .cloned()
                                .ok_or_else(|| Error::message("Choice has no name"))
                        })
                        .collect::<Result<Vec<_>>>()?
                        .into(),
                ))
            }
            if value.is_array() {
                names(value)?
            } else {
                let mut selected = JsonObject::default();
                for part in if key == JsonString::from("modelTools") {
                    ["remove", "add"]
                } else {
                    ["add", "remove"]
                } {
                    if part == "add" && key == JsonString::from("tools") {
                        continue;
                    }
                    if let Some(value) = wire::get(value, part) {
                        selected.insert(part.into(), names(value)?);
                    }
                }
                selected.into_value()
            }
        } else {
            value.clone()
        };
        state.0.insert(key, value);
    }
    Ok(())
}
pub fn configure(
    tx: &Transaction,
    conversation: ConversationId,
    change: JsonObject,
) -> StorageFuture<()> {
    let draft = tx.doc(&AGENT_DOC, conversation);
    Box::pin(async move { draft.await?.update(|state| apply_change(state, &change)) })
}
pub fn create_agent(tx: &Transaction, conversation: ConversationRecord) -> StorageFuture<()> {
    if conversation.parent.is_some() {
        return Box::pin(async { Ok(()) });
    }
    let target = tx.doc(&AGENT_DOC, conversation.id);
    let tx = tx.clone();
    Box::pin(async move {
        let target = target.await?;
        if let Some(owner) = conversation.owner {
            let owner = tx.doc(&AGENT_DOC, owner.conversation_id).await?.read()?;
            target.update(|state| {
                for (key, value) in owner.0.iter() {
                    state.0.insert(key.clone(), value.clone());
                }
                Ok(())
            })?;
        }
        Ok(())
    })
}
pub fn add_tools(
    tx: &Transaction,
    conversation: ConversationId,
    added: Vec<JsonString>,
) -> StorageFuture<()> {
    let state = tx.doc(&AGENT_DOC, conversation);
    Box::pin(async move {
        state.await?.update(|state| {
            let tools = state.0.get(&JsonString::from("tools")).cloned();
            if let Some(JsonValue::Array(mut names)) = tools {
                for name in &added {
                    let name = wire::encode(name);
                    if !names.contains(&name) {
                        std::sync::Arc::make_mut(&mut names).push(name);
                    }
                }
                state.0.insert("tools".into(), JsonValue::Array(names));
            } else if let Some(tools) = tools {
                let removed = list(wire::get(&tools, "remove"))?;
                if removed.iter().any(|name| added.contains(name)) {
                    let mut filter = JsonObject::default();
                    filter.insert(
                        "remove".into(),
                        wire::encode(
                            &removed
                                .into_iter()
                                .filter(|name| !added.contains(name))
                                .collect::<Vec<_>>(),
                        ),
                    );
                    state.0.insert("tools".into(), filter.into_value());
                }
            }
            let model = state.0.get(&JsonString::from("modelTools")).cloned();
            if let Some(JsonValue::Array(mut names)) = model {
                for name in &added {
                    let name = wire::encode(name);
                    if !names.contains(&name) {
                        std::sync::Arc::make_mut(&mut names).push(name);
                    }
                }
                state.0.insert("modelTools".into(), JsonValue::Array(names));
                return Ok(());
            }
            let remove = list(model.as_ref().and_then(|model| wire::get(model, "remove")))?
                .into_iter()
                .filter(|name| !added.contains(name))
                .collect::<Vec<_>>();
            let mut add = list(model.as_ref().and_then(|model| wire::get(model, "add")))?;
            for name in added {
                if !add.contains(&name) {
                    add.push(name);
                }
            }
            let mut filter = JsonObject::default();
            if !remove.is_empty() {
                filter.insert("remove".into(), wire::encode(&remove));
            }
            filter.insert("add".into(), wire::encode(&add));
            state.0.insert("modelTools".into(), filter.into_value());
            Ok(())
        })
    })
}
fn list(value: Option<&JsonValue>) -> Result<Vec<JsonString>> {
    value
        .map(wire::decode)
        .transpose()
        .map_err(Error::message)
        .map(Option::unwrap_or_default)
}
pub enum AgentSection<S> {
    Registered(Arc<S>),
    Instructions(JsonString),
}
impl<S: RegisteredSection> AgentSection<S> {
    pub fn key(&self) -> &JsonString {
        match self {
            Self::Registered(section) => section.key(),
            Self::Instructions(_) => {
                static KEY: LazyLock<JsonString> = LazyLock::new(|| INSTRUCTIONS_KEY.into());
                &KEY
            }
        }
    }
}
pub struct Agent<T, S> {
    pub model: Option<JsonValue>,
    pub thinking_level: JsonString,
    pub extensions: Vec<Arc<Extension<T, S>>>,
    pub tools: Vec<Arc<T>>,
    pub callable: Vec<Arc<T>>,
    pub deferred: Vec<Arc<T>>,
    pub sections: Vec<AgentSection<S>>,
    pub instructions: Option<JsonString>,
    pub cwd: Option<JsonString>,
}

#[derive(Clone)]
pub struct AgentHook {
    pub extension: JsonString,
    pub task: JsonString,
    pub handlers: Arc<dyn Any + Send + Sync>,
}
/// One resolved agent identity, retaining the host's complete registration types.
#[derive(Clone)]
pub struct AgentView {
    value: Arc<dyn Any + Send + Sync>,
    hooks: Arc<Vec<AgentHook>>,
    model: Option<JsonValue>,
    thinking_level: JsonString,
    tool: Arc<
        dyn Fn(&JsonString, bool) -> Result<Option<Arc<dyn super::tool::ExecutableTool>>>
            + Send
            + Sync,
    >,
    tool_extension: Arc<dyn Fn(&JsonString) -> Option<JsonString> + Send + Sync>,
    deferred_tools: Arc<dyn Fn() -> Result<Vec<super::declaration::ToolDeclaration>> + Send + Sync>,
    callable_tools:
        Arc<dyn Fn() -> Result<Vec<Arc<dyn super::tool::ExecutableTool>>> + Send + Sync>,
}
impl AgentView {
    pub fn tool(
        &self,
        name: &JsonString,
        nested: bool,
    ) -> Result<Option<Arc<dyn super::tool::ExecutableTool>>> {
        (self.tool)(name, nested)
    }
    pub fn tool_extension(&self, name: &JsonString) -> Option<JsonString> {
        (self.tool_extension)(name)
    }
    pub fn deferred_tools(&self) -> Result<Vec<super::declaration::ToolDeclaration>> {
        (self.deferred_tools)()
    }
    pub fn callable_tools(&self) -> Result<Vec<Arc<dyn super::tool::ExecutableTool>>> {
        (self.callable_tools)()
    }
    pub fn model(&self) -> Option<&JsonValue> {
        self.model.as_ref()
    }
    pub fn thinking_level(&self) -> &JsonString {
        &self.thinking_level
    }
    pub fn typed<T: RegisteredTool, S: RegisteredSection>(&self) -> Result<Arc<Agent<T, S>>> {
        self.value
            .clone()
            .downcast::<Agent<T, S>>()
            .map_err(|_| Error::message("Agent does not have the requested registration types"))
    }
    pub fn same_agent(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.value, &other.value)
    }
    pub fn hooks(&self, task: &JsonString) -> impl Iterator<Item = &AgentHook> {
        self.hooks.iter().filter(move |hook| hook.task == *task)
    }
}
impl<T: RegisteredTool, S: RegisteredSection> From<Arc<Agent<T, S>>> for AgentView {
    fn from(agent: Arc<Agent<T, S>>) -> Self {
        let hooks = agent
            .extensions
            .iter()
            .flat_map(|extension| {
                extension.hooks.iter().map(|hook| AgentHook {
                    extension: extension.name.clone(),
                    task: hook.task.clone(),
                    handlers: hook.handlers.clone(),
                })
            })
            .collect();
        let selected = agent.clone();
        let owner = agent.clone();
        let deferred = agent.clone();
        let callable = agent.clone();
        Self {
            tool: Arc::new(move |name, nested| {
                let tools = if nested {
                    &selected.callable
                } else {
                    &selected.tools
                };
                tools
                    .iter()
                    .find(|tool| tool.name() == name)
                    .map(|tool| {
                        tool.clone().executable().ok_or_else(|| {
                            Error::message(format!("Tool {name} has no executable capability"))
                        })
                    })
                    .transpose()
            }),
            tool_extension: Arc::new(move |name| tool_extension(&owner, name)),
            deferred_tools: Arc::new(move || {
                deferred
                    .deferred
                    .iter()
                    .map(|tool| {
                        tool.clone()
                            .executable()
                            .map(|tool| tool.declaration())
                            .ok_or_else(|| {
                                Error::message(format!(
                                    "Tool {} has no executable capability",
                                    tool.name()
                                ))
                            })
                    })
                    .collect()
            }),
            model: agent.model.clone(),
            callable_tools: Arc::new(move || {
                callable
                    .callable
                    .iter()
                    .map(|tool| {
                        tool.clone().executable().ok_or_else(|| {
                            Error::message(format!(
                                "Tool {} has no executable capability",
                                tool.name()
                            ))
                        })
                    })
                    .collect()
            }),
            thinking_level: agent.thinking_level.clone(),
            value: agent,
            hooks: Arc::new(hooks),
        }
    }
}
pub struct AgentRequest {
    pub session: Session,
    pub conversation: ConversationId,
    pub snapshot: RegistryView,
    pub settings: SettingsReader,
    pub report: Arc<dyn Fn(Error, JsonString) + Send + Sync>,
    pub context: Context,
}
pub type AgentResolver = Arc<dyn Fn(AgentRequest) -> StorageFuture<AgentView> + Send + Sync>;
/// Read committed state at first use, then read the current settings and compose
/// against the phase's snapshot. The scheduler shares this operation per phase.
pub fn resolve_agent_in(request: AgentRequest) -> StorageFuture<AgentView> {
    Box::pin(async move {
        let state = request
            .session
            .snapshot(&AGENT_DOC, request.conversation, &request.context)
            .await?;
        let settings = read_settings(&request.settings)?;
        request
            .snapshot
            .resolve_agent(state.as_deref(), &settings, |error, extension| {
                (request.report)(error, extension.clone());
                Ok(())
            })
    })
}
pub fn resolve_agent<T: RegisteredTool, S: RegisteredSection>(
    state: Option<&AgentState>,
    snapshot: &RegistrySnapshot<T, S>,
    settings: &Settings,
    mut report: impl FnMut(Error, &JsonString),
) -> Result<Agent<T, S>> {
    try_resolve_agent(state, snapshot, settings, move |error, extension| {
        report(error, extension);
        Ok(())
    })
}
/// Fallible host reporting preserves the first callback failure and stops
/// composition at that callback, just as a throwing host reporter does.
pub fn try_resolve_agent<T: RegisteredTool, S: RegisteredSection>(
    state: Option<&AgentState>,
    snapshot: &RegistrySnapshot<T, S>,
    settings: &Settings,
    mut report: impl FnMut(Error, &JsonString) -> Result<()>,
) -> Result<Agent<T, S>> {
    let state = state.map(|state| &state.0);
    let get = |key: &str| state.and_then(|state| state.get(&JsonString::from(key)));
    let stored = get("extensions");
    let selected = if let Some(JsonValue::Array(_)) = stored {
        list(stored)?
    } else {
        let base = settings.extensions.clone().unwrap_or_else(|| {
            snapshot
                .installed()
                .iter()
                .map(|extension| extension.name.clone())
                .collect()
        });
        let removed = list(stored.and_then(|value| wire::get(value, "remove")))?;
        base.into_iter()
            .chain(list(stored.and_then(|value| wire::get(value, "add")))?)
            .filter(|name| !removed.contains(name))
            .collect()
    };
    let extensions = selected
        .into_iter()
        .collect::<IndexSet<_>>()
        .into_iter()
        .filter_map(|name| snapshot.extension(&name))
        .collect::<Vec<_>>();
    let mut tools = IndexMap::new();
    let mut sections = IndexMap::new();
    for extension in &extensions {
        for tool in &extension.tools {
            tools.insert(tool.name().clone(), tool.clone());
        }
        for section in &extension.sections {
            sections.insert(section.key().clone(), section.clone());
        }
    }
    fn wrap<T>(
        items: &mut IndexMap<JsonString, Arc<T>>,
        name: &JsonString,
        apply: &(dyn Fn(Arc<T>) -> Result<Arc<T>> + Send + Sync),
        name_of: impl Fn(&T) -> &JsonString,
    ) -> Result<()> {
        let Some(item) = items.get(name).cloned() else {
            return Ok(());
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| apply(item)))
            .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)))
            .and_then(|wrapped| {
                if name_of(&wrapped) != name {
                    return Err(Error::message(format!(
                        "Wrapper renamed {name} to {}",
                        name_of(&wrapped)
                    )));
                }
                Ok(wrapped)
            });
        match result {
            Ok(item) => {
                items.insert(name.clone(), item);
                Ok(())
            }
            Err(error) => {
                items.shift_remove(name);
                Err(error)
            }
        }
    }
    for extension in &extensions {
        for wrapper in &extension.wraps {
            let result = match wrapper {
                Wrap::Tool { name, apply } => wrap(&mut tools, name, apply.as_ref(), T::name),
                Wrap::Section { key, apply } => wrap(&mut sections, key, apply.as_ref(), S::key),
            };
            if let Err(error) = result {
                report(error, &extension.name)?;
            }
        }
    }
    fn filter<T: RegisteredTool>(
        tools: Vec<Arc<T>>,
        filter: Option<&JsonValue>,
    ) -> Result<Vec<Arc<T>>> {
        let Some(filter) = filter else {
            return Ok(tools);
        };
        if filter.is_array() {
            let by_name = tools
                .into_iter()
                .map(|tool| (tool.name().clone(), tool))
                .collect::<IndexMap<_, _>>();
            return Ok(list(Some(filter))?
                .into_iter()
                .collect::<IndexSet<_>>()
                .iter()
                .filter_map(|name| by_name.get(name).cloned())
                .collect());
        }
        let removed = list(wire::get(filter, "remove"))?;
        Ok(tools
            .into_iter()
            .filter(|tool| !removed.contains(tool.name()))
            .collect())
    }
    let enabled = filter(tools.into_values().collect(), get("tools"))?;
    let by_model = enabled
        .iter()
        .filter(|tool| {
            tool.callers()
                .is_none_or(|callers| callers.contains(&ToolCaller::Model))
        })
        .cloned()
        .collect::<Vec<_>>();
    let callable = enabled
        .into_iter()
        .filter(|tool| {
            tool.callers()
                .is_none_or(|callers| callers.contains(&ToolCaller::Tools))
        })
        .collect();
    let model_filter = get("modelTools");
    let offered = if model_filter.is_some_and(JsonValue::is_array) {
        filter(by_model.clone(), model_filter)?
    } else {
        let removed = list(model_filter.and_then(|value| wire::get(value, "remove")))?;
        let mut kept = by_model
            .iter()
            .filter(|tool| !tool.deferred() && !removed.contains(tool.name()))
            .cloned()
            .collect::<Vec<_>>();
        for name in list(model_filter.and_then(|value| wire::get(value, "add")))?
            .into_iter()
            .collect::<IndexSet<_>>()
        {
            if !removed.contains(&name)
                && let Some(tool) = by_model
                    .iter()
                    .find(|tool| tool.name() == &name && tool.deferred())
            {
                kept.push(tool.clone());
            }
        }
        kept
    };
    let deferred = by_model
        .into_iter()
        .filter(|tool| tool.deferred() && !offered.iter().any(|offered| Arc::ptr_eq(tool, offered)))
        .collect();
    let instructions: Option<JsonString> = get("instructions")
        .map(wire::decode)
        .transpose()
        .map_err(Error::message)?;
    let mut sections = sections
        .into_values()
        .map(AgentSection::Registered)
        .collect::<Vec<_>>();
    if let Some(instructions) = &instructions {
        sections.push(AgentSection::Instructions(instructions.clone()));
    }
    Ok(Agent {
        model: get("model").cloned(),
        thinking_level: get("thinkingLevel")
            .map(wire::decode)
            .transpose()
            .map_err(Error::message)?
            .unwrap_or_else(|| "off".into()),
        extensions,
        tools: offered,
        callable,
        deferred,
        sections,
        instructions,
        cwd: get("cwd")
            .map(wire::decode)
            .transpose()
            .map_err(Error::message)?,
    })
}
pub fn agent_hooks<T, S>(
    agent: &Agent<T, S>,
    task: &JsonString,
) -> Vec<(JsonString, Arc<dyn Any + Send + Sync>)> {
    agent
        .extensions
        .iter()
        .flat_map(|extension| {
            extension
                .hooks
                .iter()
                .filter(|hook| hook.task == *task)
                .map(|hook| (extension.name.clone(), hook.handlers.clone()))
        })
        .collect()
}
pub fn section_extension<T, S: RegisteredSection>(
    agent: &Agent<T, S>,
    key: &JsonString,
) -> Option<JsonString> {
    agent
        .extensions
        .iter()
        .rfind(|extension| {
            extension
                .sections
                .iter()
                .any(|section| section.key() == key)
                || extension
                    .wraps
                    .iter()
                    .any(|wrap| matches!(wrap,Wrap::Section{key:target,..}if target==key))
        })
        .map(|extension| extension.name.clone())
}
pub fn tool_extension<T: RegisteredTool, S>(
    agent: &Agent<T, S>,
    name: &JsonString,
) -> Option<JsonString> {
    agent
        .extensions
        .iter()
        .rfind(|extension| {
            extension.tools.iter().any(|tool| tool.name() == name)
                || extension
                    .wraps
                    .iter()
                    .any(|wrap| matches!(wrap,Wrap::Tool{name:target,..}if target==name))
        })
        .map(|extension| extension.name.clone())
}
