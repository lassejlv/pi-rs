use super::types::*;
use crate::{wire, *};
use indexmap::IndexMap;
use pi_chord::delta::{Op, Seg, SharedJson};
use serde::de::DeserializeOwned;
use std::collections::{HashMap, HashSet};

pub type EventBatch = Vec<AgentEvent>;
#[derive(Default)]
pub struct EventTranslator {
    held: HashSet<TaskId>,
}
pub(super) fn empty_object() -> SharedJson {
    SharedJson::object([])
}
fn empty_array() -> SharedJson {
    SharedJson::array([])
}
pub(super) fn required<T: DeserializeOwned>(value: &SharedJson, name: &str) -> Result<T> {
    let value = value
        .get_key(name)
        .ok_or_else(|| Error::message(format!("Missing event field {name}")))?;
    wire::decode(&value.to_value()).map_err(Error::message)
}
fn optional<T: DeserializeOwned>(value: &SharedJson, name: &str) -> Result<Option<T>> {
    value
        .get_key(name)
        .map(|value| wire::decode(&value.to_value()).map_err(Error::message))
        .transpose()
}
fn member<'a>(value: Option<&'a SharedJson>, name: &str) -> Option<&'a SharedJson> {
    value.and_then(|value| value.get_key(name))
}
fn list(value: Option<&SharedJson>) -> &[SharedJson] {
    value.and_then(SharedJson::as_array).unwrap_or(&[])
}
fn live(root: &SharedJson) -> Option<&SharedJson> {
    member(root.get_key("docs"), "pi.live")
}
fn doc<'a>(root: &'a SharedJson, kind: &str) -> Option<&'a SharedJson> {
    member(root.get_key("docs"), kind)
}
fn nonnull(value: Option<&SharedJson>) -> Option<&SharedJson> {
    value.filter(|value| !value.as_scalar().is_some_and(JsonValue::is_null))
}
fn status(slot: &SharedJson, expected: &str) -> bool {
    slot.get_key("status")
        .and_then(SharedJson::as_scalar)
        .is_some_and(|value| value.string_equals(expected))
}
fn first(value: Option<&SharedJson>) -> Option<&SharedJson> {
    value
        .and_then(SharedJson::as_array)
        .and_then(|values| values.first())
}
fn inputs(run: &SharedJson) -> Result<SharedJson> {
    run.get_key("inputs")
        .cloned()
        .ok_or_else(|| Error::message("Run has no input list"))
}
pub(super) fn queued(inbox: Option<&SharedJson>) -> Result<Vec<QueuedItem>> {
    list(member(inbox, "items"))
        .iter()
        .map(|item| {
            Ok(QueuedItem {
                id: required(item, "id")?,
                mode: optional(item, "mode")?,
            })
        })
        .collect()
}
pub fn snapshot_of(root: &SharedJson) -> Result<SnapshotEvent> {
    let live = live(root);
    Ok(SnapshotEvent {
        tag: "snapshot",
        entries: root.get_key("entries").cloned().unwrap_or_else(empty_array),
        run: nonnull(member(live, "run"))
            .map(|run| {
                Ok::<_, Error>(RunSnapshot {
                    inputs: inputs(run)?,
                })
            })
            .transpose()?,
        generation: member(live, "generation").cloned(),
        tools: nonnull(member(live, "tools"))
            .cloned()
            .unwrap_or_else(empty_array),
        nested_tools: nonnull(member(live, "nestedTools"))
            .cloned()
            .unwrap_or_else(empty_array),
        compactions: nonnull(member(live, "compactions"))
            .cloned()
            .unwrap_or_else(empty_array),
        compaction_failure: member(live, "compactionFailure").cloned(),
        inbox: queued(doc(root, "pi.inbox"))?,
        agent: nonnull(doc(root, "pi.agent"))
            .cloned()
            .unwrap_or_else(empty_object),
        usage: nonnull(doc(root, "pi.usage")).cloned().unwrap_or_else(|| {
            SharedJson::new(wire::encode(&super::super::usage::UsageState::default()))
        }),
    })
}
fn call_of(slot: &SharedJson) -> Result<ToolEventCall> {
    Ok(ToolEventCall {
        tool_call_id: required(slot, "callId")?,
        tool_name: required(slot, "name")?,
        task_id: optional(slot, "taskId")?,
        parent: if slot.get_key("parentCallId").is_some() {
            Some(ParentToolCall {
                parent_tool_call_id: required(slot, "parentCallId")?,
                parent_task_id: required(slot, "parentTaskId")?,
            })
        } else {
            None
        },
    })
}
fn path(op: &Op) -> &[Seg] {
    op.path().unwrap_or(&[])
}
pub fn message_changes(ops: &[Op], message: &SharedJson) -> Result<Vec<MessageChange>> {
    let partial = vec![
        "docs".into(),
        "pi.live".into(),
        "generation".into(),
        "message".into(),
    ];
    let mut changes = Vec::new();
    let mut whole = HashSet::new();
    for op in ops {
        let path = path(op);
        if !path.starts_with(&partial) {
            if partial.starts_with(path) {
                return Ok(vec![MessageChange::Message {
                    message: message.clone(),
                }]);
            }
            continue;
        }
        let rest = &path[partial.len()..];
        if rest.first() == Some(&Seg::from("usage")) {
            continue;
        }
        if rest.first() != Some(&Seg::from("content")) {
            return Ok(vec![MessageChange::Message {
                message: message.clone(),
            }]);
        }
        if rest.len() == 1 {
            let Op::Splice(_, index, 0, blocks) = op else {
                return Ok(vec![MessageChange::Message {
                    message: message.clone(),
                }]);
            };
            for (offset, block) in blocks.iter().enumerate() {
                let content_index = index + offset;
                let block = SharedJson::new(block.clone());
                changes.push(
                    if block
                        .get_key("type")
                        .and_then(SharedJson::as_scalar)
                        .is_some_and(|value| value.string_equals("text"))
                    {
                        MessageChange::TextStart {
                            content_index,
                            block,
                        }
                    } else if block
                        .get_key("type")
                        .and_then(SharedJson::as_scalar)
                        .is_some_and(|value| value.string_equals("thinking"))
                    {
                        MessageChange::ThinkingStart {
                            content_index,
                            block,
                        }
                    } else {
                        MessageChange::ToolcallStart {
                            content_index,
                            block,
                        }
                    },
                );
            }
            continue;
        }
        let Some(Seg::Index(content_index)) = rest.get(1) else {
            return Err(Error::message("Message content index is not numeric"));
        };
        let content_index = *content_index;
        if whole.contains(&content_index) {
            continue;
        }
        match op {
            Op::Append(_, delta) if rest.len() == 3 && rest.get(2) == Some(&Seg::from("text")) => {
                changes.push(MessageChange::TextDelta {
                    content_index,
                    delta: delta.clone(),
                })
            }
            Op::Append(_, delta)
                if rest.len() == 3 && rest.get(2) == Some(&Seg::from("thinking")) =>
            {
                changes.push(MessageChange::ThinkingDelta {
                    content_index,
                    delta: delta.clone(),
                })
            }
            Op::Append(_, delta) if rest.get(2) == Some(&Seg::from("arguments")) => {
                changes.push(MessageChange::ToolcallDelta {
                    content_index,
                    path: rest[3..].to_vec(),
                    delta: delta.clone(),
                })
            }
            _ => {
                whole.insert(content_index);
                let block = message
                    .get_key("content")
                    .and_then(SharedJson::as_array)
                    .and_then(|items| items.get(content_index))
                    .ok_or_else(|| Error::message("Message content block is missing"))?;
                changes.push(MessageChange::Block {
                    content_index,
                    block: block.clone(),
                });
            }
        }
    }
    Ok(changes)
}
fn text(value: Option<&SharedJson>) -> Option<&JsonString> {
    value
        .and_then(SharedJson::as_scalar)
        .and_then(JsonValue::as_string)
}
fn tool_update(
    ops: &[Op],
    kind: &str,
    index: usize,
    slot: &SharedJson,
    previous: &SharedJson,
) -> Option<ToolUpdate> {
    let at = vec![
        "docs".into(),
        "pi.live".into(),
        kind.into(),
        index.into(),
        "output".into(),
    ];
    let mut trim = 0;
    let mut append = JsonString::default();
    let mut set = false;
    for op in ops {
        if !path(op).starts_with(&at) {
            continue;
        }
        match op {
            Op::Truncate(_, count) => trim += count,
            Op::Append(_, suffix) => append.append(suffix),
            _ => set = true,
        }
    }
    let output = if set
        || (text(slot.get_key("output")) != text(previous.get_key("output"))
            && trim == 0
            && append.units().is_empty())
    {
        Some(OutputChange::Set {
            set: text(slot.get_key("output")).cloned().unwrap_or_default(),
        })
    } else if trim > 0 || !append.units().is_empty() {
        Some(OutputChange::Window {
            trim_start: (trim > 0).then_some(trim),
            append: (!append.units().is_empty()).then_some(append),
        })
    } else {
        None
    };
    let details =
        (!SharedJson::same_optional(slot.get_key("details"), previous.get_key("details"))).then(
            || {
                slot.get_key("details")
                    .cloned()
                    .unwrap_or_else(|| SharedJson::new(JsonValue::Null))
            },
        );
    let diagnostics =
        (!SharedJson::same_optional(slot.get_key("diagnostics"), previous.get_key("diagnostics")))
            .then(|| {
                slot.get_key("diagnostics")
                    .cloned()
                    .unwrap_or_else(empty_array)
            });
    if output.is_none() && details.is_none() && diagnostics.is_none() {
        None
    } else {
        Some(ToolUpdate {
            output,
            details,
            diagnostics,
        })
    }
}
impl EventTranslator {
    pub fn new(held: impl IntoIterator<Item = TaskId>) -> Self {
        Self {
            held: held.into_iter().collect(),
        }
    }
    pub fn translate(
        &mut self,
        conversation: ConversationId,
        before: &SharedJson,
        after: &SharedJson,
        ops: &[Op],
        publication: &CommitPublication,
    ) -> Result<EventBatch> {
        let mut entries = Vec::new();
        let mut tasks = IndexMap::new();
        let mut submissions = Vec::new();
        let mut nested_results = HashMap::new();
        for change in &publication.changes {
            match change {
                CommitChange::Document(DocumentCommitChange::Document {
                    record,
                    conversation_id: Some(id),
                    value,
                    ..
                }) if *id == conversation
                    && record.create.kind.equals_str("pi.tool.nested-call")
                    && !value.is_null() =>
                {
                    let value = SharedJson::new(value.as_ref().clone());
                    if let (Some(task), Some(result)) = (
                        optional::<TaskId>(&value, "taskId")?,
                        value.get_key("result"),
                    ) {
                        nested_results.insert(task, result.clone());
                    }
                }
                CommitChange::Entry(entry) if entry.conversation_id == conversation => {
                    let value = list(after.get_key("entries"))
                        .iter()
                        .find(|value| {
                            optional::<EntryId>(value, "id").ok().flatten() == Some(entry.id)
                        })
                        .cloned()
                        .unwrap_or_else(|| SharedJson::new(wire::encode(entry)));
                    entries.push((entry.id, value));
                }
                CommitChange::Task(task) if task.info.conversation_id == conversation => {
                    tasks.insert(task.id(), task.as_ref());
                }
                CommitChange::Submission(record) if record.info.conversation_id == conversation => {
                    submissions.push(record.clone())
                }
                _ => {}
            }
        }
        if ops.is_empty() && entries.is_empty() && tasks.is_empty() && submissions.is_empty() {
            return Ok(Vec::new());
        }
        submissions.sort_by_key(SubmissionRecord::id);
        let was = live(before);
        let now = live(after);
        let slots_before = list(member(was, "tools"));
        let slots = list(member(now, "tools"));
        let nested_before = list(member(was, "nestedTools"));
        let nested = list(member(now, "nestedTools"));
        let by_call = |slots: &[SharedJson]| -> Result<IndexMap<JsonString, SharedJson>> {
            slots
                .iter()
                .map(|slot| Ok((required(slot, "callId")?, slot.clone())))
                .collect()
        };
        let by_task = |slots: &[SharedJson]| -> Result<IndexMap<TaskId, SharedJson>> {
            slots
                .iter()
                .map(|slot| Ok((required(slot, "taskId")?, slot.clone())))
                .collect()
        };
        let prior_slots = by_call(slots_before)?;
        let prior_nested = by_task(nested_before)?;
        let mut events = Vec::new();
        for slot in slots.iter().chain(nested) {
            let call = call_of(slot)?;
            let previous = if call.parent.is_some() {
                call.task_id.and_then(|id| prior_nested.get(&id))
            } else {
                prior_slots.get(&call.tool_call_id)
            };
            if !status(slot, "running") || previous.is_some_and(|slot| status(slot, "running")) {
                continue;
            }
            let args = if call.parent.is_some() {
                slot.get_key("arguments")
                    .cloned()
                    .ok_or_else(|| Error::message("Nested slot has no arguments"))?
            } else {
                call.task_id
                    .and_then(|id| tasks.get(&id))
                    .and_then(|record| record.state.checkpoint())
                    .and_then(|checkpoint| wire::get(checkpoint, "arguments"))
                    .filter(|value| !value.is_null())
                    .map(|value| SharedJson::new(value.clone()))
                    .unwrap_or_else(empty_object)
            };
            events.push(AgentEvent::ToolExecutionStart { call, args });
        }
        let generation_before = member(was, "generation");
        let generation = member(now, "generation");
        let partial_before = member(generation_before, "message");
        let partial = member(generation, "message");
        if let Some(partial) = partial {
            if partial_before.is_none() {
                events.push(AgentEvent::MessageStart {
                    message: partial.clone(),
                });
            } else if !SharedJson::same_optional(Some(partial), partial_before) {
                events.push(AgentEvent::MessageUpdate {
                    usage: partial
                        .get_key("usage")
                        .cloned()
                        .ok_or_else(|| Error::message("Partial message has no usage"))?,
                    changes: message_changes(ops, partial)?,
                });
            }
        }
        for (kind, slots) in [("tools", slots), ("nestedTools", nested)] {
            for (index, slot) in slots.iter().enumerate() {
                let call = call_of(slot)?;
                let previous = if kind == "tools" {
                    prior_slots.get(&call.tool_call_id)
                } else {
                    call.task_id.and_then(|id| prior_nested.get(&id))
                };
                if let Some(previous) = previous.filter(|previous| status(previous, "running"))
                    && status(slot, "running")
                    && let Some(update) = tool_update(ops, kind, index, slot, previous)
                {
                    events.push(AgentEvent::ToolExecutionUpdate { call, update });
                }
            }
        }
        let retry = member(generation, "retry");
        let retry_before = member(generation_before, "retry");
        if let Some(retry) = retry
            && retry_before.is_none()
        {
            let generation = generation.expect("retry has a generation");
            events.push(AgentEvent::AutoRetryStart {
                attempt: required(generation, "attempt")?,
                at: required(retry, "at")?,
                error_message: required(retry, "error")?,
            });
        }
        if retry_before.is_some() && retry.is_none() {
            events.push(AgentEvent::AutoRetryEnd {
                attempt: required(
                    generation_before.expect("retry has a generation"),
                    "attempt",
                )?,
            });
        }
        let deferred = member(generation, "deferred");
        let deferred_before = member(generation_before, "deferred");
        if let Some(deferred) = deferred
            && !SharedJson::same_optional(
                deferred.get_key("pollAt"),
                member(deferred_before, "pollAt"),
            )
        {
            events.push(AgentEvent::DeferredPoll {
                poll_at: required(deferred, "pollAt")?,
            });
        }
        let result_of = |call: &JsonString| {
            entries.iter().find_map(|(id, entry)| {
                let message = first(entry.get_key("model"))?;
                (message
                    .get_key("role")
                    .and_then(SharedJson::as_scalar)
                    .is_some_and(|value| value.string_equals("toolResult"))
                    && text(message.get_key("toolCallId")) == Some(call))
                .then_some(*id)
            })
        };
        let mut tool_ends = Vec::new();
        let mut end_model = |slot: &SharedJson, id: Option<EntryId>| -> Result<()> {
            let entry = id
                .and_then(|id| entries.iter().find(|(candidate, _)| *candidate == id))
                .map(|(_, entry)| entry.clone());
            tool_ends.push((call_of(slot)?, entry));
            Ok(())
        };
        for (id, previous) in &prior_slots {
            if status(previous, "done") {
                continue;
            }
            match slots
                .iter()
                .find(|slot| text(slot.get_key("callId")) == Some(id))
            {
                Some(slot) if status(slot, "done") => end_model(slot, optional(slot, "entry")?)?,
                None => end_model(previous, result_of(id))?,
                _ => {}
            }
        }
        for slot in slots {
            if status(slot, "done")
                && !prior_slots.contains_key(&required::<JsonString>(slot, "callId")?)
            {
                end_model(slot, optional(slot, "entry")?)?;
            }
        }
        let current_nested = by_task(nested)?;
        let mut end_nested = |slot: &SharedJson| -> Result<()> {
            let call = call_of(slot)?;
            let receipt = call
                .task_id
                .and_then(|id| nested_results.get(&id))
                .cloned()
                .map(|result| ToolEnd::Nested { result })
                .unwrap_or(ToolEnd::Missing {});
            events.push(AgentEvent::ToolExecutionEnd { call, receipt });
            Ok(())
        };
        for (id, previous) in prior_nested.iter().rev() {
            if status(previous, "done") {
                continue;
            }
            let current = current_nested.get(id);
            if current.is_none() || current.is_some_and(|slot| status(slot, "done")) {
                end_nested(current.unwrap_or(previous))?;
            }
        }
        for slot in nested.iter().rev() {
            if status(slot, "done")
                && !prior_nested.contains_key(&required::<TaskId>(slot, "taskId")?)
            {
                end_nested(slot)?;
            }
        }
        let mut assistant_appended = false;
        for (_, entry) in &entries {
            for (call, result) in &tool_ends {
                if result.as_ref().is_some_and(|result| result.ptr_eq(entry)) {
                    events.push(AgentEvent::ToolExecutionEnd {
                        call: call.clone(),
                        receipt: ToolEnd::Model {
                            entry: entry.clone(),
                        },
                    });
                }
            }
            let Some(message) = first(entry.get_key("model")) else {
                events.push(AgentEvent::EntryAppended {
                    entry: entry.clone(),
                });
                continue;
            };
            let assistant = message
                .get_key("role")
                .and_then(SharedJson::as_scalar)
                .is_some_and(|value| value.string_equals("assistant"));
            let streamed = assistant && partial_before.is_some() && !assistant_appended;
            assistant_appended |= assistant;
            if !streamed {
                events.push(AgentEvent::MessageStart {
                    message: message.clone(),
                });
            }
            events.push(AgentEvent::MessageEnd {
                entry: entry.clone(),
            });
        }
        for (call, entry) in tool_ends {
            if entry.is_none() {
                events.push(AgentEvent::ToolExecutionEnd {
                    call,
                    receipt: ToolEnd::Missing {},
                });
            }
        }
        let compactions_before = list(member(was, "compactions"));
        let compactions = list(member(now, "compactions"));
        let failure = member(now, "compactionFailure");
        for slot in compactions_before {
            let task_id: TaskId = required(slot, "taskId")?;
            if !compactions
                .iter()
                .any(|slot| optional::<TaskId>(slot, "taskId").ok().flatten() == Some(task_id))
            {
                let error = if failure
                    .map(|value| optional::<TaskId>(value, "taskId"))
                    .transpose()?
                    .flatten()
                    == Some(task_id)
                {
                    failure
                        .map(|value| optional::<JsonString>(value, "error"))
                        .transpose()?
                        .flatten()
                } else {
                    None
                };
                events.push(AgentEvent::CompactionEnd {
                    task_id,
                    reason: slot
                        .get_key("reason")
                        .cloned()
                        .ok_or_else(|| Error::message("Compaction has no reason"))?,
                    error,
                });
            }
        }
        let mut turn_ended = false;
        for task in tasks.values() {
            let generation = task.info.kind.equals_str("pi.generation");
            if generation
                && task.state.status() == TaskStatus::Completing
                && self.held.insert(task.id())
            {
                turn_ended = true;
            }
            if let TaskState::Terminal { outcome } = &task.state {
                if generation && !self.held.remove(&task.id()) {
                    turn_ended = true;
                }
                let message = match outcome {
                    TaskOutcome::Faulted { error } => Some(error.message.clone()),
                    TaskOutcome::Orphaned { reason } => Some(reason.clone()),
                    _ => None,
                };
                if let Some(message) = message {
                    events.push(AgentEvent::TaskFailed {
                        task_id: task.id(),
                        kind: task.info.kind.clone(),
                        message,
                    });
                }
            }
        }
        if turn_ended {
            events.push(AgentEvent::TurnEnd);
        }
        let run = member(now, "run");
        let run_before = member(was, "run");
        let run_changed = !SharedJson::same_optional(
            first(member(run, "inputs")),
            first(member(run_before, "inputs")),
        );
        if let Some(run) = run_before
            && run_changed
        {
            events.push(AgentEvent::RunEnd {
                inputs: inputs(run)?,
            });
        }
        events.extend(
            submissions
                .into_iter()
                .map(|record| AgentEvent::Submission { record }),
        );
        if !SharedJson::same_optional(doc(after, "pi.inbox"), doc(before, "pi.inbox")) {
            events.push(AgentEvent::InboxUpdate {
                items: queued(doc(after, "pi.inbox"))?,
            });
        }
        if !SharedJson::same_optional(doc(after, "pi.agent"), doc(before, "pi.agent")) {
            events.push(AgentEvent::AgentChanged {
                agent: nonnull(doc(after, "pi.agent"))
                    .cloned()
                    .unwrap_or_else(empty_object),
            });
        }
        if !SharedJson::same_optional(doc(after, "pi.usage"), doc(before, "pi.usage")) {
            events.push(AgentEvent::UsageChanged {
                usage: nonnull(doc(after, "pi.usage")).cloned().unwrap_or_else(|| {
                    SharedJson::new(wire::encode(&super::super::usage::UsageState::default()))
                }),
            });
        }
        for slot in compactions {
            let task_id: TaskId = required(slot, "taskId")?;
            if !compactions_before
                .iter()
                .any(|slot| optional::<TaskId>(slot, "taskId").ok().flatten() == Some(task_id))
            {
                events.push(AgentEvent::CompactionStart {
                    task_id,
                    reason: slot
                        .get_key("reason")
                        .cloned()
                        .ok_or_else(|| Error::message("Compaction has no reason"))?,
                    blocking: required(slot, "blocking")?,
                });
            }
        }
        if let Some(run) = run {
            if run_changed {
                events.push(AgentEvent::RunStart {
                    inputs: inputs(run)?,
                });
            }
            if !SharedJson::same_optional(run.get_key("taskId"), member(run_before, "taskId"))
                && optional::<TaskId>(run, "taskId")?
                    .and_then(|id| tasks.get(&id))
                    .is_some_and(|task| task.info.kind.equals_str("pi.generation"))
            {
                events.push(AgentEvent::TurnStart);
            }
        }
        Ok(events)
    }
}
