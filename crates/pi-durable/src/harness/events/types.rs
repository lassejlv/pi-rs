//! Message and provider wire records stay lossless and retain immutable child
//! identity; event discriminants and lifecycle identifiers are native types.
use crate::*;
use pi_chord::delta::{Seg, SharedJson};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotEvent {
    #[serde(rename = "type")]
    pub(super) tag: &'static str,
    pub entries: SharedJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<RunSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<SharedJson>,
    pub tools: SharedJson,
    pub nested_tools: SharedJson,
    pub compactions: SharedJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction_failure: Option<SharedJson>,
    pub inbox: Vec<QueuedItem>,
    pub agent: SharedJson,
    pub usage: SharedJson,
}
#[derive(Clone, Debug, Serialize)]
pub struct RunSnapshot {
    pub inputs: SharedJson,
}
#[derive(Clone, Debug, Serialize)]
pub struct QueuedItem {
    pub id: SubmissionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<super::super::inbox::InputMode>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolEventCall {
    pub tool_call_id: JsonString,
    pub tool_name: JsonString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ParentToolCall>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentToolCall {
    pub parent_tool_call_id: JsonString,
    pub parent_task_id: TaskId,
}
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum ToolEnd {
    Model { entry: SharedJson },
    Nested { result: SharedJson },
    Missing {},
}
#[derive(Clone, Debug, Serialize)]
#[serde(untagged, rename_all_fields = "camelCase")]
pub enum OutputChange {
    Set {
        set: JsonString,
    },
    Window {
        #[serde(skip_serializing_if = "Option::is_none")]
        trim_start: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        append: Option<JsonString>,
    },
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct ToolUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<SharedJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<SharedJson>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum MessageChange {
    TextStart {
        content_index: usize,
        block: SharedJson,
    },
    ThinkingStart {
        content_index: usize,
        block: SharedJson,
    },
    #[serde(rename = "toolcall_start")]
    ToolcallStart {
        content_index: usize,
        block: SharedJson,
    },
    TextDelta {
        content_index: usize,
        delta: JsonString,
    },
    ThinkingDelta {
        content_index: usize,
        delta: JsonString,
    },
    #[serde(rename = "toolcall_delta")]
    ToolcallDelta {
        content_index: usize,
        path: Vec<Seg>,
        delta: JsonString,
    },
    Block {
        content_index: usize,
        block: SharedJson,
    },
    Message {
        message: SharedJson,
    },
}
#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentEvent {
    RunStart {
        inputs: SharedJson,
    },
    RunEnd {
        inputs: SharedJson,
    },
    TurnStart,
    TurnEnd,
    MessageStart {
        message: SharedJson,
    },
    MessageUpdate {
        usage: SharedJson,
        changes: Vec<MessageChange>,
    },
    MessageEnd {
        entry: SharedJson,
    },
    ToolExecutionStart {
        #[serde(flatten)]
        call: ToolEventCall,
        args: SharedJson,
    },
    ToolExecutionUpdate {
        #[serde(flatten)]
        call: ToolEventCall,
        #[serde(flatten)]
        update: ToolUpdate,
    },
    ToolExecutionEnd {
        #[serde(flatten)]
        call: ToolEventCall,
        #[serde(flatten)]
        receipt: ToolEnd,
    },
    InboxUpdate {
        items: Vec<QueuedItem>,
    },
    Submission {
        record: SubmissionRecord,
    },
    AutoRetryStart {
        attempt: f64,
        at: f64,
        error_message: JsonString,
    },
    AutoRetryEnd {
        attempt: f64,
    },
    DeferredPoll {
        poll_at: f64,
    },
    EntryAppended {
        entry: SharedJson,
    },
    AgentChanged {
        agent: SharedJson,
    },
    UsageChanged {
        usage: SharedJson,
    },
    TaskFailed {
        task_id: TaskId,
        kind: JsonString,
        message: JsonString,
    },
    CompactionStart {
        task_id: TaskId,
        reason: SharedJson,
        blocking: bool,
    },
    CompactionEnd {
        task_id: TaskId,
        reason: SharedJson,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<JsonString>,
    },
    #[serde(untagged)]
    Snapshot(SnapshotEvent),
}
