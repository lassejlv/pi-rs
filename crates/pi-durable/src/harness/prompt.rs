//! Positional prompt rendering and exact section/tool loadout preparation.
use super::{
    agent::AgentSection,
    context::ContextView,
    declaration::{ToolDeclaration, current_tool_declarations},
    registry::RegisteredSection,
};
use crate::{
    storage::{StorageFuture, ready},
    wire, *,
};
use indexmap::{IndexMap, IndexSet};
use std::sync::Arc;

pub type Sections = IndexMap<JsonString, JsonString>;
type Patch = IndexMap<JsonString, Option<JsonString>>;
pub struct PromptInput<A: ?Sized, R, E: ?Sized = ()> {
    pub conversation_id: ConversationId,
    pub agent: Arc<A>,
    pub env: Option<Arc<E>>,
    pub shown: Sections,
    pub read: R,
}
pub trait PromptSection<I>: RegisteredSection {
    fn render(&self, input: Arc<I>, context: &Context) -> StorageFuture<Option<JsonString>>;
    fn tagged(&self) -> bool {
        true
    }
}
pub type NativePromptInput =
    super::generation::GenerationPromptInput<super::tool::NativeTool, NativeSection>;
pub type NativeSectionRender = std::sync::Arc<
    dyn Fn(std::sync::Arc<NativePromptInput>, Context) -> StorageFuture<Option<JsonString>>
        + Send
        + Sync,
>;
/// Common native registration type, so built-in and caller sections compose.
pub struct NativeSection {
    pub key: JsonString,
    pub tagged: bool,
    pub render: NativeSectionRender,
}
impl RegisteredSection for NativeSection {
    fn key(&self) -> &JsonString {
        &self.key
    }
}
impl PromptSection<NativePromptInput> for NativeSection {
    fn render(
        &self,
        input: Arc<NativePromptInput>,
        context: &Context,
    ) -> StorageFuture<Option<JsonString>> {
        (self.render)(input, context.clone())
    }
    fn tagged(&self) -> bool {
        self.tagged
    }
}
impl<S: RegisteredSection> RegisteredSection for AgentSection<S> {
    fn key(&self) -> &JsonString {
        AgentSection::key(self)
    }
}
impl<I, S: PromptSection<I>> PromptSection<I> for AgentSection<S> {
    fn render(&self, input: Arc<I>, context: &Context) -> StorageFuture<Option<JsonString>> {
        match self {
            Self::Registered(section) => section.render(input, context),
            Self::Instructions(text) => ready(Ok(Some(text.clone()))),
        }
    }
    fn tagged(&self) -> bool {
        match self {
            Self::Registered(section) => section.tagged(),
            // Upstream gives the instructions section no `tag: false`, so it renders wrapped.
            Self::Instructions(_) => true,
        }
    }
}
pub fn replay_sections(messages: &[Arc<JsonValue>]) -> Result<Sections> {
    let mut shown = Sections::new();
    for message in messages {
        if !message["role"].string_equals("system") {
            continue;
        }
        let Some(sections) = wire::get(message, "sections") else {
            continue;
        };
        let sections = sections
            .as_object()
            .ok_or_else(|| Error::message("Expected prompt sections object"))?;
        for (key, value) in pi_chord::json::object_entries(sections) {
            if value.is_null() {
                shown.shift_remove(key);
            } else {
                shown.insert(key.clone(), wire::decode(value).map_err(Error::message)?);
            }
        }
    }
    Ok(shown)
}
pub async fn render_sections<I, S: PromptSection<I>>(
    sections: &[S],
    input: Arc<I>,
    shown: &Sections,
    mut report: impl FnMut(Error, &S) -> Result<()>,
    context: &Context,
) -> Result<Sections> {
    let mut desired = Sections::new();
    for section in sections {
        let text = match section.render(input.clone(), context).await {
            Ok(text) => text,
            Err(error) => {
                if context
                    .abort_signal()
                    .is_some_and(|signal| signal.aborted())
                {
                    return Err(error);
                }
                report(error, section)?;
                if let Some(text) = shown.get(section.key()) {
                    desired.insert(section.key().clone(), text.clone());
                }
                continue;
            }
        };
        let Some(text) = text else {
            continue;
        };
        let text = if section.tagged() {
            let mut units = Vec::new();
            units.extend("<".encode_utf16());
            units.extend_from_slice(section.key().units());
            units.extend(">\n".encode_utf16());
            units.extend_from_slice(text.units());
            units.extend("\n</".encode_utf16());
            units.extend_from_slice(section.key().units());
            units.extend(">".encode_utf16());
            JsonString::from_utf16(units)
        } else {
            text
        };
        desired.insert(section.key().clone(), text);
    }
    Ok(desired)
}
pub fn plan_system_entries(
    view: &ContextView,
    desired: &Sections,
    tools: &[ToolDeclaration],
    timestamp: f64,
) -> Result<Vec<EntryDraft>> {
    if let Some(head) = &view.head
        && !view
            .entries
            .iter()
            .any(|entry| entry.kind == *SYSTEM_ENTRY.kind() && entry.id > head.id)
    {
        let edits = view
            .entries
            .iter()
            .filter(|entry| entry.kind == *SYSTEM_ENTRY.kind())
            .map(|entry| ContextEdit::Omit { target: entry.id })
            .collect::<Vec<_>>();
        let patch = desired
            .iter()
            .map(|(key, value)| (key.clone(), Some(value.clone())))
            .collect();
        let changes = ToolChanges {
            removed: Vec::new(),
            added: tools.to_vec(),
        };
        let mut baseline = system_entry(Some(&patch), Some(&changes), timestamp);
        if !edits.is_empty() {
            baseline.edits = Some(edits);
        }
        return Ok(vec![baseline]);
    }
    let sections = plan_sections(&replay_sections(&view.messages)?, desired);
    let changes = plan_tools(&current_tool_declarations(&view.messages)?, tools);
    if changes.removed.is_empty() && changes.added.is_empty() {
        return Ok(sections
            .iter()
            .map(|patch| system_entry(Some(patch), None, timestamp))
            .collect());
    }
    if sections.is_empty() {
        return Ok(vec![system_entry(None, Some(&changes), timestamp)]);
    }
    Ok(sections
        .iter()
        .enumerate()
        .map(|(i, patch)| {
            system_entry(
                Some(patch),
                if i + 1 == sections.len() {
                    Some(&changes)
                } else {
                    None
                },
                timestamp,
            )
        })
        .collect())
}
struct ToolChanges {
    removed: Vec<JsonString>,
    added: Vec<ToolDeclaration>,
}
fn plan_tools(offered: &[ToolDeclaration], desired: &[ToolDeclaration]) -> ToolChanges {
    let wanted = desired
        .iter()
        .map(|tool| (&tool.name, tool))
        .collect::<IndexMap<_, _>>();
    let kept = offered
        .iter()
        .filter(|tool| {
            wanted
                .get(&tool.name)
                .is_some_and(|next| tool.same_declaration(next))
        })
        .collect::<Vec<_>>();
    let names = kept.iter().map(|tool| &tool.name).collect::<IndexSet<_>>();
    let added = desired
        .iter()
        .filter(|tool| !names.contains(&tool.name))
        .cloned()
        .collect::<Vec<_>>();
    let replayed = kept
        .iter()
        .map(|tool| &tool.name)
        .chain(added.iter().map(|tool| &tool.name));
    if replayed
        .enumerate()
        .any(|(i, name)| desired.get(i).is_none_or(|tool| &tool.name != name))
    {
        ToolChanges {
            removed: offered.iter().map(|tool| tool.name.clone()).collect(),
            added: desired.to_vec(),
        }
    } else {
        ToolChanges {
            removed: offered
                .iter()
                .filter(|tool| !names.contains(&tool.name))
                .map(|tool| tool.name.clone())
                .collect(),
            added,
        }
    }
}
fn plan_sections(shown: &Sections, desired: &Sections) -> Vec<Patch> {
    let patched_order = shown
        .keys()
        .filter(|key| desired.contains_key(*key))
        .chain(desired.keys().filter(|key| !shown.contains_key(*key)));
    if patched_order.ne(desired.keys()) {
        return vec![
            shown.keys().map(|key| (key.clone(), None)).collect(),
            desired
                .iter()
                .map(|(key, value)| (key.clone(), Some(value.clone())))
                .collect(),
        ];
    }
    let mut patch = Patch::new();
    // The reference's minimal patch uses ordinary property assignment. The
    // __proto__ setter creates no own key for its string/null payloads.
    for (key, value) in shown {
        let next = desired.get(key);
        if next != Some(value) && !key.equals_str("__proto__") {
            patch.insert(key.clone(), next.cloned());
        }
    }
    for (key, value) in desired {
        if !shown.contains_key(key) && !key.equals_str("__proto__") {
            patch.insert(key.clone(), Some(value.clone()));
        }
    }
    if patch.is_empty() {
        Vec::new()
    } else {
        vec![patch]
    }
}
fn system_entry(
    sections: Option<&Patch>,
    tools: Option<&ToolChanges>,
    timestamp: f64,
) -> EntryDraft {
    let mut message = wire::object();
    wire::put(&mut message, "role", &"system");
    wire::put(&mut message, "content", &"");
    if let Some(sections) = sections {
        let patch = JsonValue::Object(
            sections
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        value.as_ref().map(wire::encode).unwrap_or(JsonValue::Null),
                    )
                })
                .collect::<pi_chord::json::Map>()
                .into(),
        );
        wire::put(&mut message, "sections", &patch);
    }
    if let Some(tools) = tools {
        if !tools.removed.is_empty() {
            let removed = tools
                .removed
                .iter()
                .map(|name| {
                    let mut value = wire::object();
                    wire::put(&mut value, "name", name);
                    value
                })
                .collect::<Vec<_>>();
            wire::put(&mut message, "toolsRemoved", &removed);
        }
        if !tools.added.is_empty() {
            wire::put(&mut message, "toolsAdded", &tools.added);
        }
    }
    wire::put(&mut message, "timestamp", &timestamp);
    let mut draft = EntryDraft::new(SYSTEM_ENTRY.kind().clone());
    draft.model = Some(vec![message]);
    draft
}
