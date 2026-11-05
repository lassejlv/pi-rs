//! Shared live conversation presentation and ownership of each run's inputs.
use super::usage::{UsageBucket, record_usage};
use crate::{wire, *};
use pi_chord::delta::{Op, Seg};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotStatus {
    Pending,
    Running,
    Done,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Info,
    Warn,
    Error,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDiagnostic {
    pub severity: DiagnosticSeverity,
    pub message: JsonString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<JsonString>,
}
#[derive(Clone, Debug)]
pub struct SlotProgress {
    pub call_id: JsonString,
    pub name: JsonString,
    pub status: SlotStatus,
    pub output: Option<JsonString>,
    pub dropped_bytes: Option<u64>,
    pub dropped_lines: Option<u64>,
    pub details: Option<JsonValue>,
    pub diagnostics: Option<Vec<ToolDiagnostic>>,
}
impl SlotProgress {
    pub fn new(call_id: impl Into<JsonString>, name: impl Into<JsonString>) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            status: SlotStatus::Pending,
            output: None,
            dropped_bytes: None,
            dropped_lines: None,
            details: None,
            diagnostics: None,
        }
    }
    fn to_json(&self) -> JsonValue {
        let mut value = wire::object();
        wire::put(&mut value, "callId", &self.call_id);
        wire::put(&mut value, "name", &self.name);
        wire::put(&mut value, "status", &self.status);
        wire::put_optional(&mut value, "output", &self.output);
        wire::put_optional(&mut value, "droppedBytes", &self.dropped_bytes);
        wire::put_optional(&mut value, "droppedLines", &self.dropped_lines);
        wire::put_optional(&mut value, "details", &self.details);
        wire::put_optional(&mut value, "diagnostics", &self.diagnostics);
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        Ok(Self {
            call_id: wire::field(value, "callId")?,
            name: wire::field(value, "name")?,
            status: wire::field(value, "status")?,
            output: wire::optional(value, "output")?,
            dropped_bytes: wire::optional(value, "droppedBytes")?,
            dropped_lines: wire::optional(value, "droppedLines")?,
            details: wire::optional(value, "details")?,
            diagnostics: wire::optional(value, "diagnostics")?,
        })
    }
}
wire::raw_serde!(SlotProgress);
#[derive(Clone, Debug)]
pub struct ToolSlot {
    pub progress: SlotProgress,
    pub task_id: Option<TaskId>,
    pub entry: Option<EntryId>,
}
impl ToolSlot {
    fn to_json(&self) -> JsonValue {
        let mut value = self.progress.to_json();
        wire::put_optional(&mut value, "taskId", &self.task_id);
        wire::put_optional(&mut value, "entry", &self.entry);
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        Ok(Self {
            progress: SlotProgress::from_json(value)?,
            task_id: wire::optional(value, "taskId")?,
            entry: wire::optional(value, "entry")?,
        })
    }
}
wire::raw_serde!(ToolSlot);
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolSummary {
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<JsonObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonString>,
}
#[derive(Clone, Debug)]
pub struct NestedToolSlot {
    pub progress: SlotProgress,
    pub parent_call_id: JsonString,
    pub parent_task_id: TaskId,
    pub task_id: TaskId,
    pub arguments: JsonValue,
    pub summary: Option<NestedToolSummary>,
}
impl NestedToolSlot {
    fn to_json(&self) -> JsonValue {
        let mut value = self.progress.to_json();
        wire::put(&mut value, "parentCallId", &self.parent_call_id);
        wire::put(&mut value, "parentTaskId", &self.parent_task_id);
        wire::put(&mut value, "taskId", &self.task_id);
        wire::put(&mut value, "arguments", &self.arguments);
        wire::put_optional(&mut value, "summary", &self.summary);
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        Ok(Self {
            progress: SlotProgress::from_json(value)?,
            parent_call_id: wire::field(value, "parentCallId")?,
            parent_task_id: wire::field(value, "parentTaskId")?,
            task_id: wire::field(value, "taskId")?,
            arguments: wire::field(value, "arguments")?,
            summary: wire::optional(value, "summary")?,
        })
    }
}
wire::raw_serde!(NestedToolSlot);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetryStatus {
    pub at: f64,
    pub error: JsonString,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeferredStatus {
    #[serde(rename = "pollAt")]
    pub poll_at: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStatus {
    pub task_id: TaskId,
    pub reason: CompactionReason,
    pub blocking: bool,
    pub attempt: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryStatus>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionFailure {
    pub task_id: TaskId,
    pub reason: CompactionReason,
    pub error: JsonString,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveRun {
    pub task_id: TaskId,
    pub inputs: Vec<SubmissionId>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveGeneration {
    pub attempt: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<JsonObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredStatus>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<LiveRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<LiveGeneration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSlot>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nested_tools: Option<Vec<NestedToolSlot>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compactions: Option<Vec<CompactionStatus>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_failure: Option<CompactionFailure>,
}
impl DocumentState for LiveState {}
pub static LIVE_DOC: std::sync::LazyLock<LatestConversationDocToken<LiveState>> =
    std::sync::LazyLock::new(|| {
        define_doc(
            DocDefinition::new(
                "pi.live",
                1,
                LatestConversationScope {
                    fork: LatestFork::Initial,
                },
                || Ok(LiveState::default()),
                |_, _, _| unreachable!("the raw checkpoint decides"),
            )
            .raw_checkpoint(|raw, _, _| {
                // Same rule as the typed state: checkpoint once no generation or tool is in flight. Reading the stored
                // JSON keeps a commit from decoding every slot of the live document.
                let raw = raw.value();
                let running = |slots: &JsonValue| {
                    slots.as_array().is_some_and(|slots| {
                        slots
                            .iter()
                            .any(|slot| slot["status"].string_equals("running"))
                    })
                };
                raw["generation"].is_null()
                    && !running(&raw["tools"])
                    && !running(&raw["nestedTools"])
            }),
        )
        .expect("valid built-in live definition")
    });
/// The fields of the live document that hold compaction progress.
pub const COMPACTION_FIELDS: &[&str] = &["compactions", "compactionFailure"];
pub fn live_doc() -> LatestConversationDocToken<LiveState> {
    LIVE_DOC.clone()
}
pub fn end_run(
    tx: &Transaction,
    live: &DocDraft<LiveState>,
    task: TaskId,
    settlement: SubmissionSettlement,
) -> Result<()> {
    let (run, present) = live.with_json(|raw| -> Result<_> {
        let run = if raw["run"].is_null() {
            None
        } else {
            Some(wire::decode::<LiveRun>(&raw["run"]).map_err(Error::message)?)
        };
        let present = |key: &str| !raw[key].is_null();
        Ok((run, ["generation", "tools", "nestedTools"].map(present)))
    })??;
    let mut ops = Vec::new();
    if let Some(run) = run.filter(|run| run.task_id == task) {
        for id in run.inputs {
            tx.settle_submission(id, settlement.clone())?;
        }
        ops.push(Op::Delete(vec!["run".into()]));
    }
    for (key, present) in ["generation", "tools", "nestedTools"]
        .into_iter()
        .zip(present)
    {
        if present {
            ops.push(Op::Delete(vec![key.into()]));
        }
    }
    live.apply_ops(&ops)
}
pub fn add_compaction_status(live: &mut LiveState, status: CompactionStatus) {
    live.compactions.get_or_insert_default().push(status);
}
pub fn compaction_status(live: &mut LiveState, task: TaskId) -> Option<&mut CompactionStatus> {
    live.compactions
        .as_mut()?
        .iter_mut()
        .find(|status| status.task_id == task)
}
pub fn remove_compaction_status(live: &mut LiveState, task: TaskId) {
    if let Some(statuses) = &mut live.compactions {
        if let Some(index) = statuses.iter().position(|status| status.task_id == task) {
            statuses.remove(index);
        }
        if statuses.is_empty() {
            live.compactions = None;
        }
    }
}
pub fn fail_compaction_status(live: &mut LiveState, task: TaskId, error: JsonString) {
    if let Some(status) = compaction_status(live, task) {
        live.compaction_failure = Some(CompactionFailure {
            task_id: status.task_id,
            reason: status.reason,
            error,
        });
    }
    remove_compaction_status(live, task);
}
pub enum ToolSlotMut<'a> {
    Model(&'a mut ToolSlot),
    Nested(&'a mut NestedToolSlot),
}
impl ToolSlotMut<'_> {
    pub fn progress(&mut self) -> &mut SlotProgress {
        match self {
            Self::Model(slot) => &mut slot.progress,
            Self::Nested(slot) => &mut slot.progress,
        }
    }
}
pub fn tool_slot(live: &mut LiveState, task: TaskId) -> Option<ToolSlotMut<'_>> {
    if let Some(slot) = live
        .tools
        .as_mut()
        .and_then(|slots| slots.iter_mut().find(|slot| slot.task_id == Some(task)))
    {
        return Some(ToolSlotMut::Model(slot));
    }
    let nested = live.nested_tools.as_mut()?;
    let index = nested
        .binary_search_by_key(&task, |slot| slot.task_id)
        .ok()?;
    Some(ToolSlotMut::Nested(&mut nested[index]))
}
pub fn finish_slot(slot: &mut SlotProgress) {
    slot.status = SlotStatus::Done;
    clear_progress(slot);
}
pub fn clear_progress(slot: &mut SlotProgress) {
    slot.output = None;
    slot.dropped_bytes = None;
    slot.dropped_lines = None;
    slot.details = None;
    slot.diagnostics = None;
}
pub fn remove_nested_slots(live: &mut LiveState, task: TaskId) {
    let Some(nested) = &mut live.nested_tools else {
        return;
    };
    let mut removed = std::collections::HashSet::from([task]);
    for slot in nested.iter() {
        if removed.contains(&slot.parent_task_id) {
            removed.insert(slot.task_id);
        }
    }
    nested.retain(|slot| !removed.contains(&slot.parent_task_id));
    if nested.is_empty() {
        live.nested_tools = None;
    }
}
pub async fn convert_partial(
    tx: &Transaction,
    live: &DocDraft<LiveState>,
    conversation: ConversationId,
) -> Result<()> {
    let Some(partial) = live
        .with_json(|raw| JsonObject::try_from_value(raw["generation"]["message"].clone()).ok())?
    else {
        return Ok(());
    };
    let mut message = partial.into_value();
    wire::put(&mut message, "stopReason", &"aborted");
    let usage = wire::field(&message, "usage").map_err(Error::message)?;
    let provider: JsonString = wire::field(&message, "provider").map_err(Error::message)?;
    let model: JsonString = wire::field(&message, "model").map_err(Error::message)?;
    let mut key = provider.units().to_vec();
    key.push(b'/' as u16);
    key.extend_from_slice(model.units());
    record_usage(
        tx,
        conversation,
        UsageBucket::Models,
        JsonString::from_utf16(key),
        &usage,
    )
    .await?;
    let mut entry = EntryDraft::new(ASSISTANT_ENTRY.kind().clone());
    entry.model = Some(vec![message]);
    tx.append_entry(conversation, entry).await?;
    Ok(())
}
pub async fn settle_scheduler_outcome(
    tx: Transaction,
    record: TaskRecord,
    outcome: TaskOutcome,
) -> Result<()> {
    let kind = &record.info.kind;
    if !kind.equals_str("pi.tool")
        && !kind.equals_str("pi.compaction")
        && !kind.equals_str("pi.generation")
    {
        return Ok(());
    }
    let live = tx.doc(&live_doc(), record.info.conversation_id).await?;
    if kind.equals_str("pi.tool") {
        return update_fields(&live, &["tools", "nestedTools"], |state| {
            if let Some(mut slot) = tool_slot(state, record.id()) {
                finish_slot(slot.progress());
            }
            remove_nested_slots(state, record.id());
        });
    }
    if kind.equals_str("pi.compaction") {
        let error = match outcome {
            TaskOutcome::Faulted { error } => error.message,
            TaskOutcome::Orphaned { reason } => reason,
            _ => return Ok(()),
        };
        return update_fields(&live, COMPACTION_FIELDS, |state| {
            fail_compaction_status(state, record.id(), error);
        });
    }
    let running =
        live.with_json(|raw| raw["run"]["taskId"].as_f64() == Some(record.id().get() as f64))?;
    if !running {
        return Ok(());
    }
    convert_partial(&tx, &live, record.info.conversation_id).await?;
    let settlement = match outcome {
        TaskOutcome::Faulted { error } => SubmissionSettlement::Unanswered {
            reason: "faulted".into(),
            detail: Some(JsonValue::String(error.message)),
        },
        TaskOutcome::Orphaned { reason } => SubmissionSettlement::Unanswered {
            reason,
            detail: None,
        },
        _ => return Ok(()),
    };
    end_run(&tx, &live, record.id(), settlement)
}

/// Where a tool slot sits in the stored live document.
#[derive(Clone, Copy, Debug)]
pub struct SlotLocation {
    pub field: &'static str,
    pub index: usize,
}
/// Find the slot of `task` in the stored JSON: model slots by scan, nested slots (sorted by task) by search.
pub fn locate_slot(raw: &JsonValue, task: TaskId) -> Option<SlotLocation> {
    let id = task.get() as f64;
    let key = |slot: &JsonValue| slot["taskId"].as_f64().unwrap_or(f64::NAN);
    if let Some(index) = raw["tools"]
        .as_array()
        .and_then(|slots| slots.iter().position(|slot| key(slot) == id))
    {
        return Some(SlotLocation {
            field: "tools",
            index,
        });
    }
    let nested = raw["nestedTools"].as_array()?;
    let index = nested
        .binary_search_by(|slot| {
            key(slot)
                .partial_cmp(&id)
                .unwrap_or(std::cmp::Ordering::Less)
        })
        .ok()
        .or_else(|| nested.iter().position(|slot| key(slot) == id))?;
    Some(SlotLocation {
        field: "nestedTools",
        index,
    })
}
pub fn slot_json(raw: &JsonValue, at: SlotLocation) -> &JsonValue {
    &raw[at.field][at.index]
}
/// The progress part of a stored slot.
pub fn slot_progress(
    raw: &JsonValue,
    at: SlotLocation,
) -> std::result::Result<SlotProgress, String> {
    SlotProgress::from_json(slot_json(raw, at))
}
pub fn slot_path(at: SlotLocation, key: &str) -> Vec<Seg> {
    vec![Seg::from(at.field), Seg::Index(at.index), Seg::from(key)]
}
/// Set one field of a stored slot unless it already holds that value.
pub fn set_slot_field(
    raw: &JsonValue,
    at: SlotLocation,
    key: &str,
    value: JsonValue,
    ops: &mut Vec<Op>,
) {
    let current = &slot_json(raw, at)[key];
    let present = slot_json(raw, at)
        .as_object()
        .is_some_and(|slot| slot.contains_key(&JsonString::from(key)));
    if !present || *current != value {
        ops.push(Op::Set(slot_path(at, key), value));
    }
}
/// Operations that mark a stored slot done and drop its streaming fields, like `finish_slot`.
pub fn finish_slot_ops(raw: &JsonValue, at: SlotLocation) -> Vec<Op> {
    let mut ops = Vec::new();
    set_slot_field(raw, at, "status", wire::encode(&SlotStatus::Done), &mut ops);
    ops.extend(clear_progress_ops(raw, at));
    ops
}
/// Operations that remove the streaming fields of a stored slot, like `clear_progress`.
pub fn clear_progress_ops(raw: &JsonValue, at: SlotLocation) -> Vec<Op> {
    let slot = slot_json(raw, at).as_object();
    [
        "output",
        "droppedBytes",
        "droppedLines",
        "details",
        "diagnostics",
    ]
    .into_iter()
    .filter(|key| slot.is_some_and(|slot| slot.contains_key(&JsonString::from(*key))))
    .map(|key| Op::Delete(slot_path(at, key)))
    .collect()
}
/// The operation that appends a nested slot, creating the list when absent.
pub fn push_nested_slot_op(raw: &JsonValue, slot: &NestedToolSlot) -> Op {
    let encoded = wire::encode(slot);
    match raw["nestedTools"].as_array() {
        Some(slots) => Op::Splice(
            vec![Seg::from("nestedTools")],
            slots.len(),
            0,
            vec![encoded],
        ),
        None => Op::Set(
            vec![Seg::from("nestedTools")],
            JsonValue::Array(vec![encoded].into()),
        ),
    }
}

/// Apply a typed update that only touches the named top-level fields of the live document. Only those fields are
/// decoded, and only the ones that changed are written back, as fine-grained operations; the other fields (above
/// all the nested tool slots, which can be many and large) are never decoded or encoded.
pub fn update_fields(
    live: &DocDraft<LiveState>,
    fields: &[&'static str],
    update: impl FnOnce(&mut LiveState),
) -> Result<()> {
    let (mut partial, before) = live.with_json(|raw| {
        let mut subset = wire::object();
        let mut before = Vec::new();
        for field in fields {
            let value = raw
                .as_object()
                .and_then(|object| object.get(&JsonString::from(*field)));
            before.push(value.cloned());
            if let Some(value) = value {
                subset
                    .as_object_mut()
                    .expect("object")
                    .insert((*field).into(), value.clone());
            }
        }
        wire::decode::<LiveState>(&subset)
            .map(|partial| (partial, before))
            .map_err(Error::message)
    })??;
    update(&mut partial);
    let after = wire::encode(&partial);
    let mut ops = Vec::new();
    for (field, before) in fields.iter().zip(&before) {
        let path = [Seg::from(*field)];
        match (before, wire::get(&after, field)) {
            (None, None) => {}
            (Some(_), None) => ops.push(Op::Delete(path.to_vec())),
            (before, Some(after)) => {
                ops.extend(super::json::assign_json_ops(before.as_ref(), after, &path))
            }
        }
    }
    if ops.is_empty() {
        Ok(())
    } else {
        live.apply_ops(&ops)
    }
}
