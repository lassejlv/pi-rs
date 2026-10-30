use crate::wire::{self, raw_serde};
use pi_chord::{JsonString, JsonValue, delta::Op};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{DeserializeOwned, Visitor},
};
use std::sync::Arc;
use std::{
    fmt,
    ops::{Deref, DerefMut},
};

pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
macro_rules! id_type {
    ($($name:ident),+) => {$(
        #[derive(Clone,Copy,Debug,Default,PartialEq,Eq,PartialOrd,Ord,Hash,Serialize)]
        #[serde(transparent)]
        pub struct $name(i64);
        impl $name {
            /// Trusted allocation/decoding boundary; IDs share a numeric namespace,
            /// but record kinds remain distinct Rust types.
            pub const fn from_raw(value:i64)->Self {Self(value)}
            pub const fn get(self)->i64 {self.0}
        }
        impl fmt::Display for $name {fn fmt(&self,f:&mut fmt::Formatter<'_>)->fmt::Result {self.0.fmt(f)}}
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D:Deserializer<'de>>(d:D)->std::result::Result<Self,D::Error> {
                struct Number;
                impl Visitor<'_> for Number {
                    type Value=$name;
                    fn expecting(&self,f:&mut fmt::Formatter<'_>)->fmt::Result {f.write_str("a safe integer")}
                    fn visit_i64<E:serde::de::Error>(self,n:i64)->std::result::Result<$name,E> {self.visit_f64(n as f64)}
                    fn visit_u64<E:serde::de::Error>(self,n:u64)->std::result::Result<$name,E> {self.visit_f64(n as f64)}
                    fn visit_f64<E:serde::de::Error>(self,n:f64)->std::result::Result<$name,E> {
                        if n.is_finite() && n.fract()==0.0 && n.abs()<=MAX_SAFE_INTEGER as f64 {Ok($name(n as i64))} else {Err(E::custom("expected a safe integer"))}
                    }
                }
                d.deserialize_any(Number)
            }
        }
    )+};
}
id_type!(
    ConversationId,
    EntryId,
    TaskId,
    SubmissionId,
    DocumentId,
    Seq
);
pub const ROOT_CONVERSATION_ID: ConversationId = ConversationId::from_raw(1);

/// Document roots are objects, including keys that contain arbitrary UTF-16 units.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonObject(JsonValue);
impl Default for JsonObject {
    fn default() -> Self {
        Self(wire::object())
    }
}
impl JsonObject {
    pub fn new(map: pi_chord::json::Map) -> Self {
        Self(JsonValue::Object(map.into()))
    }
    pub fn try_from_value(value: JsonValue) -> std::result::Result<Self, String> {
        if value.is_object() {
            Ok(Self(value))
        } else {
            Err("Expected a JSON object".into())
        }
    }
    pub fn value(&self) -> &JsonValue {
        &self.0
    }
    pub fn into_value(self) -> JsonValue {
        self.0
    }
}
impl Deref for JsonObject {
    type Target = pi_chord::json::Map;
    fn deref(&self) -> &Self::Target {
        self.0.as_object().unwrap()
    }
}
impl DerefMut for JsonObject {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_object_mut().unwrap()
    }
}
impl Serialize for JsonObject {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}
impl<'de> Deserialize<'de> for JsonObject {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::try_from_value(JsonValue::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
fn present_json<'de, D: Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<JsonValue>, D::Error> {
    JsonValue::deserialize(d).map(Some)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationParent {
    pub conversation_id: ConversationId,
    pub at: EntryId,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationOwner {
    pub conversation_id: ConversationId,
    pub task_id: TaskId,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConversationRecord {
    pub id: ConversationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ConversationParent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<ConversationOwner>,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum ContextEdit {
    Omit {
        target: EntryId,
    },
    Replace {
        target: EntryId,
        messages: Vec<JsonValue>,
    },
}
impl<'de> Deserialize<'de> for ContextEdit {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let value = JsonValue::deserialize(d)?;
        let decode = || -> std::result::Result<Self, String> {
            let target = wire::field(&value, "target")?;
            match wire::tag(&value, "action")?.as_str() {
                "omit" => Ok(Self::Omit { target }),
                "replace" => Ok(Self::Replace {
                    target,
                    messages: wire::field(&value, "messages")?,
                }),
                other => Err(format!("Unknown context edit {other}")),
            }
        };
        decode().map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryRecord {
    pub id: EntryId,
    pub conversation_id: ConversationId,
    pub kind: JsonString,
    /// Provider message wire records remain lossless until the provider boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Vec<JsonValue>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_json"
    )]
    pub data: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<EntryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<Vec<ContextEdit>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_task_id: Option<TaskId>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JoinPolicy {
    FailFast,
    AllSettled,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskOutcomeError {
    pub message: JsonString,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_json"
    )]
    pub detail: Option<JsonValue>,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum TaskOutcome<R = JsonValue> {
    Completed {
        result: R,
    },
    Failed {
        error: TaskOutcomeError,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<R>,
    },
    Aborted {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<JsonString>,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<R>,
    },
    Orphaned {
        reason: JsonString,
    },
    Faulted {
        error: TaskOutcomeError,
    },
}
impl<'de, R: DeserializeOwned> Deserialize<'de> for TaskOutcome<R> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let v = JsonValue::deserialize(d)?;
        let decode = || -> std::result::Result<Self, String> {
            Ok(match wire::tag(&v, "status")?.as_str() {
                "completed" => Self::Completed {
                    result: wire::field(&v, "result")?,
                },
                "failed" => Self::Failed {
                    error: wire::field(&v, "error")?,
                    result: wire::optional(&v, "result")?,
                },
                "aborted" => Self::Aborted {
                    reason: wire::optional(&v, "reason")?,
                    result: wire::optional(&v, "result")?,
                },
                "orphaned" => Self::Orphaned {
                    reason: wire::field(&v, "reason")?,
                },
                "faulted" => Self::Faulted {
                    error: wire::field(&v, "error")?,
                },
                other => return Err(format!("Unknown task outcome {other}")),
            })
        };
        decode().map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Running,
    Waiting,
    Sleeping,
    Completing,
    Terminal,
}
#[derive(Clone, Debug, PartialEq)]
pub enum TaskState<S = JsonValue, R = JsonValue> {
    Pending {
        checkpoint: S,
        memos: Option<JsonObject>,
    },
    Running {
        checkpoint: S,
        memos: Option<JsonObject>,
    },
    Waiting {
        checkpoint: S,
        on: Vec<TaskId>,
        policy: JoinPolicy,
        memos: Option<JsonObject>,
    },
    Sleeping {
        checkpoint: S,
        until: f64,
        memos: Option<JsonObject>,
    },
    Completing {
        outcome: TaskOutcome<R>,
    },
    Terminal {
        outcome: TaskOutcome<R>,
    },
}
impl<S, R> TaskState<S, R> {
    pub fn pending(checkpoint: S) -> Self {
        Self::Pending {
            checkpoint,
            memos: None,
        }
    }
    pub fn status(&self) -> TaskStatus {
        match self {
            Self::Pending { .. } => TaskStatus::Pending,
            Self::Running { .. } => TaskStatus::Running,
            Self::Waiting { .. } => TaskStatus::Waiting,
            Self::Sleeping { .. } => TaskStatus::Sleeping,
            Self::Completing { .. } => TaskStatus::Completing,
            Self::Terminal { .. } => TaskStatus::Terminal,
        }
    }
    pub fn memos(&self) -> Option<&JsonObject> {
        match self {
            Self::Pending { memos, .. }
            | Self::Running { memos, .. }
            | Self::Waiting { memos, .. }
            | Self::Sleeping { memos, .. } => memos.as_ref(),
            _ => None,
        }
    }
    pub fn checkpoint(&self) -> Option<&S> {
        match self {
            Self::Pending { checkpoint, .. }
            | Self::Running { checkpoint, .. }
            | Self::Waiting { checkpoint, .. }
            | Self::Sleeping { checkpoint, .. } => Some(checkpoint),
            _ => None,
        }
    }
}
impl<S: Serialize, R: Serialize> TaskState<S, R> {
    fn to_json(&self) -> std::result::Result<JsonValue, String> {
        let mut v = wire::object();
        wire::put(&mut v, "status", &self.status());
        if let Some(checkpoint) = self.checkpoint() {
            v.as_object_mut()
                .unwrap()
                .insert("checkpoint".into(), pi_chord::json::to_value(checkpoint)?);
        }
        match self {
            Self::Waiting { on, policy, .. } => {
                wire::put(&mut v, "on", on);
                wire::put(&mut v, "policy", policy);
            }
            Self::Sleeping { until, .. } => wire::put(&mut v, "until", until),
            Self::Completing { outcome } | Self::Terminal { outcome } => {
                v.as_object_mut()
                    .unwrap()
                    .insert("outcome".into(), pi_chord::json::to_value(outcome)?);
            }
            _ => {}
        }
        Ok(v)
    }
}
impl<S: DeserializeOwned, R: DeserializeOwned> TaskState<S, R> {
    fn from_json(v: &JsonValue) -> std::result::Result<Self, String> {
        Self::with_memos(v, None)
    }
    fn with_memos(v: &JsonValue, memos: Option<JsonObject>) -> std::result::Result<Self, String> {
        Ok(match wire::tag(v, "status")?.as_str() {
            "pending" => Self::Pending {
                checkpoint: wire::field(v, "checkpoint")?,
                memos,
            },
            "running" => Self::Running {
                checkpoint: wire::field(v, "checkpoint")?,
                memos,
            },
            "waiting" => Self::Waiting {
                checkpoint: wire::field(v, "checkpoint")?,
                on: wire::field(v, "on")?,
                policy: wire::field(v, "policy")?,
                memos,
            },
            "sleeping" => Self::Sleeping {
                checkpoint: wire::field(v, "checkpoint")?,
                until: wire::field(v, "until")?,
                memos,
            },
            "completing" | "terminal" if memos.is_some() => {
                return Err("Settled tasks cannot retain memos".into());
            }
            "completing" => Self::Completing {
                outcome: wire::field(v, "outcome")?,
            },
            "terminal" => Self::Terminal {
                outcome: wire::field(v, "outcome")?,
            },
            other => return Err(format!("Unknown task status {other}")),
        })
    }
}
impl<S: Serialize, R: Serialize> Serialize for TaskState<S, R> {
    fn serialize<T: Serializer>(&self, serializer: T) -> std::result::Result<T::Ok, T::Error> {
        self.to_json()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}
impl<'de, S: DeserializeOwned, R: DeserializeOwned> Deserialize<'de> for TaskState<S, R> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::from_json(&JsonValue::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecordInfo<I = JsonValue> {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<JsonObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<JsonObject>>,
    pub id: TaskId,
    pub conversation_id: ConversationId,
    pub kind: JsonString,
    pub version: u64,
    pub input: I,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<TaskId>,
    pub background: bool,
    pub abort_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_reason: Option<AbortReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandon_on_restart: Option<True>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarts: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AbortReason {
    Restart,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct True;
impl Serialize for True {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_bool(true)
    }
}
impl<'de> Deserialize<'de> for True {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        if bool::deserialize(d)? {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("expected true"))
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct TaskRecord<I = JsonValue, S = JsonValue, R = JsonValue> {
    pub info: TaskRecordInfo<I>,
    pub state: TaskState<S, R>,
}
impl<I, S, R> TaskRecord<I, S, R> {
    pub fn id(&self) -> TaskId {
        self.info.id
    }
}
impl<I: Serialize, S: Serialize, R: Serialize> TaskRecord<I, S, R> {
    fn to_json(&self) -> std::result::Result<JsonValue, String> {
        let mut v = pi_chord::json::to_value(&self.info)?;
        v.as_object_mut()
            .unwrap()
            .insert("state".into(), pi_chord::json::to_value(&self.state)?);
        if let Some(memos) = self.state.memos() {
            wire::put(&mut v, "memos", memos);
        }
        Ok(v)
    }
}
impl<I: DeserializeOwned, S: DeserializeOwned, R: DeserializeOwned> TaskRecord<I, S, R> {
    fn from_json(v: &JsonValue) -> std::result::Result<Self, String> {
        Ok(Self {
            info: wire::decode(v)?,
            state: TaskState::with_memos(
                wire::get(v, "state").ok_or("Missing field state")?,
                wire::optional(v, "memos")?,
            )?,
        })
    }
}
impl<I: Serialize, S: Serialize, R: Serialize> Serialize for TaskRecord<I, S, R> {
    fn serialize<T: Serializer>(&self, serializer: T) -> std::result::Result<T::Ok, T::Error> {
        self.to_json()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}
impl<'de, I: DeserializeOwned, S: DeserializeOwned, R: DeserializeOwned> Deserialize<'de>
    for TaskRecord<I, S, R>
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::from_json(&JsonValue::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubmissionStatus {
    Queued,
    Placed,
    Done,
    Unanswered,
}
#[derive(Clone, Debug, PartialEq)]
pub enum SubmissionState {
    InputQueued,
    InputPlaced {
        entry: EntryId,
    },
    InputDone {
        entry: EntryId,
        answer: EntryId,
    },
    InputUnanswered {
        entry: Option<EntryId>,
        reason: JsonString,
        detail: Option<JsonValue>,
    },
    WriteQueued,
    WriteDone {
        entry: EntryId,
    },
    WriteUnanswered {
        reason: JsonString,
        detail: Option<JsonValue>,
    },
}
impl SubmissionState {
    pub fn status(&self) -> SubmissionStatus {
        match self {
            Self::InputQueued | Self::WriteQueued => SubmissionStatus::Queued,
            Self::InputPlaced { .. } => SubmissionStatus::Placed,
            Self::InputDone { .. } | Self::WriteDone { .. } => SubmissionStatus::Done,
            _ => SubmissionStatus::Unanswered,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InputQueued
            | Self::InputPlaced { .. }
            | Self::InputDone { .. }
            | Self::InputUnanswered { .. } => "input",
            _ => "write",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionRecordInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<JsonObject>,
    pub id: SubmissionId,
    pub conversation_id: ConversationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<JsonString>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct SubmissionRecord {
    pub info: SubmissionRecordInfo,
    pub state: SubmissionState,
}
impl SubmissionRecord {
    pub fn id(&self) -> SubmissionId {
        self.info.id
    }
}
impl SubmissionState {
    fn to_json(&self) -> JsonValue {
        let mut v = wire::object();
        wire::put(&mut v, "type", self.kind());
        wire::put(&mut v, "status", &self.status());
        match self {
            SubmissionState::InputPlaced { entry } | SubmissionState::WriteDone { entry } => {
                wire::put(&mut v, "entry", entry)
            }
            SubmissionState::InputDone { entry, answer } => {
                wire::put(&mut v, "entry", entry);
                wire::put(&mut v, "answer", answer);
            }
            SubmissionState::InputUnanswered {
                entry,
                reason,
                detail,
            } => {
                wire::put_optional(&mut v, "entry", entry);
                wire::put(&mut v, "reason", reason);
                wire::put_optional(&mut v, "detail", detail);
            }
            SubmissionState::WriteUnanswered { reason, detail } => {
                wire::put(&mut v, "reason", reason);
                wire::put_optional(&mut v, "detail", detail);
            }
            _ => {}
        }
        v
    }
    fn from_json(v: &JsonValue) -> std::result::Result<Self, String> {
        let kind = wire::tag(v, "type")?;
        let status = wire::tag(v, "status")?;
        let state = match (kind.as_str(), status.as_str()) {
            ("input", "queued") => SubmissionState::InputQueued,
            ("input", "placed") => SubmissionState::InputPlaced {
                entry: wire::field(v, "entry")?,
            },
            ("input", "done") => SubmissionState::InputDone {
                entry: wire::field(v, "entry")?,
                answer: wire::field(v, "answer")?,
            },
            ("input", "unanswered") => SubmissionState::InputUnanswered {
                entry: wire::optional(v, "entry")?,
                reason: wire::field(v, "reason")?,
                detail: wire::optional(v, "detail")?,
            },
            ("write", "queued") => SubmissionState::WriteQueued,
            ("write", "done") => SubmissionState::WriteDone {
                entry: wire::field(v, "entry")?,
            },
            ("write", "unanswered") => SubmissionState::WriteUnanswered {
                reason: wire::field(v, "reason")?,
                detail: wire::optional(v, "detail")?,
            },
            _ => return Err(format!("Invalid submission {kind}/{status}")),
        };
        Ok(state)
    }
}
raw_serde!(SubmissionState);
impl SubmissionRecord {
    fn to_json(&self) -> JsonValue {
        let mut value = wire::encode(&self.info);
        let JsonValue::Object(state) = wire::encode(&self.state) else {
            unreachable!("submission state is an object")
        };
        value
            .as_object_mut()
            .unwrap()
            .extend(std::sync::Arc::unwrap_or_clone(state));
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        Ok(Self {
            info: wire::decode(value)?,
            state: wire::decode(value)?,
        })
    }
}
raw_serde!(SubmissionRecord);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DocumentScope {
    Session,
    Conversation {
        #[serde(rename = "conversationId")]
        conversation_id: ConversationId,
    },
    Task {
        #[serde(rename = "taskId")]
        task_id: TaskId,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LatestFork {
    Current,
    Initial,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ForkPolicy {
    AsOf,
    Current,
    Initial,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationHistory {
    Latest { fork: LatestFork },
    Rewindable { fork: ForkPolicy },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocumentSemantics {
    Session,
    Task {
        task_id: TaskId,
    },
    Conversation {
        conversation_id: ConversationId,
        history: ConversationHistory,
    },
}
impl DocumentSemantics {
    pub fn scope(&self) -> DocumentScope {
        match self {
            Self::Session => DocumentScope::Session,
            Self::Task { task_id } => DocumentScope::Task { task_id: *task_id },
            Self::Conversation {
                conversation_id, ..
            } => DocumentScope::Conversation {
                conversation_id: *conversation_id,
            },
        }
    }
    pub fn current_only(&self) -> bool {
        !matches!(
            self,
            Self::Conversation {
                history: ConversationHistory::Rewindable { .. },
                ..
            }
        )
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentCreate {
    pub id: DocumentId,
    pub kind: JsonString,
    pub key: Option<JsonString>,
    pub semantics: DocumentSemantics,
}
impl DocumentCreate {
    pub fn address(&self) -> DocumentAddress {
        DocumentAddress {
            kind: self.kind.clone(),
            scope: self.semantics.scope(),
            key: self.key.clone(),
        }
    }
    fn to_json(&self) -> JsonValue {
        let mut v = wire::object();
        wire::put(&mut v, "id", &self.id);
        wire::put(&mut v, "kind", &self.kind);
        wire::put_optional(&mut v, "key", &self.key);
        wire::put(&mut v, "scope", &self.semantics.scope());
        if let DocumentSemantics::Conversation { history, .. } = &self.semantics {
            match history {
                ConversationHistory::Latest { fork } => {
                    wire::put(&mut v, "history", "latest");
                    wire::put(&mut v, "fork", fork);
                }
                ConversationHistory::Rewindable { fork } => {
                    wire::put(&mut v, "history", "rewindable");
                    wire::put(&mut v, "fork", fork);
                }
            }
        }
        v
    }
    fn from_json(v: &JsonValue) -> std::result::Result<Self, String> {
        let semantics = match wire::field(v, "scope")? {
            DocumentScope::Session => DocumentSemantics::Session,
            DocumentScope::Task { task_id } => DocumentSemantics::Task { task_id },
            DocumentScope::Conversation { conversation_id } => DocumentSemantics::Conversation {
                conversation_id,
                history: match wire::tag(v, "history")?.as_str() {
                    "latest" => ConversationHistory::Latest {
                        fork: wire::field(v, "fork")?,
                    },
                    "rewindable" => ConversationHistory::Rewindable {
                        fork: wire::field(v, "fork")?,
                    },
                    other => return Err(format!("Invalid document history {other}")),
                },
            },
        };
        Ok(Self {
            id: wire::field(v, "id")?,
            kind: wire::field(v, "kind")?,
            key: wire::optional(v, "key")?,
            semantics,
        })
    }
}
raw_serde!(DocumentCreate);
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentRecord {
    pub create: DocumentCreate,
    pub created_at: Seq,
    pub retired_at: Option<Seq>,
}
impl DocumentRecord {
    pub fn id(&self) -> DocumentId {
        self.create.id
    }
    pub fn alive_at(&self, at: DocumentPoint) -> bool {
        match at {
            DocumentPoint::Current => self.retired_at.is_none(),
            DocumentPoint::At(seq) => {
                self.created_at <= seq && self.retired_at.is_none_or(|retired| seq < retired)
            }
        }
    }
    fn to_json(&self) -> JsonValue {
        let mut v = self.create.to_json();
        wire::put(&mut v, "createdAt", &self.created_at);
        wire::put_optional(&mut v, "retiredAt", &self.retired_at);
        v
    }
    fn from_json(v: &JsonValue) -> std::result::Result<Self, String> {
        Ok(Self {
            create: DocumentCreate::from_json(v)?,
            created_at: wire::field(v, "createdAt")?,
            retired_at: wire::optional(v, "retiredAt")?,
        })
    }
}
raw_serde!(DocumentRecord);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentPoint {
    Current,
    At(Seq),
}
impl Serialize for DocumentPoint {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Current => s.serialize_str("current"),
            Self::At(seq) => seq.serialize(s),
        }
    }
}
impl<'de> Deserialize<'de> for DocumentPoint {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let v = JsonValue::deserialize(d)?;
        if v.string_equals("current") {
            Ok(Self::Current)
        } else {
            wire::decode(&v)
                .map(Self::At)
                .map_err(serde::de::Error::custom)
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocumentAddress {
    pub kind: JsonString,
    pub scope: DocumentScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<JsonString>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentBase {
    pub version: u64,
    pub value: JsonObject,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DocumentContent {
    Base { version: u64, value: JsonObject },
    Delta { version: u64, ops: Arc<Vec<Op>> },
}
impl DocumentContent {
    pub fn version(&self) -> u64 {
        match self {
            Self::Base { version, .. } | Self::Delta { version, .. } => *version,
        }
    }
}
impl<'de> Deserialize<'de> for DocumentContent {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let v = JsonValue::deserialize(d)?;
        let decode = || -> std::result::Result<Self, String> {
            let version = wire::field(&v, "version")?;
            Ok(match wire::tag(&v, "kind")?.as_str() {
                "base" => Self::Base {
                    version,
                    value: wire::field(&v, "value")?,
                },
                "delta" => Self::Delta {
                    version,
                    ops: wire::field(&v, "ops")?,
                },
                other => return Err(format!("Invalid document content {other}")),
            })
        };
        decode().map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DocumentCopySource {
    pub id: DocumentId,
    pub at: DocumentPoint,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredDocument {
    pub record: DocumentRecord,
    pub version: u64,
    pub value: JsonObject,
    pub deltas_since_base: usize,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum StorageWrite {
    #[serde(rename = "conversation")]
    Conversation { value: ConversationRecord },
    #[serde(rename = "entry")]
    Entry { value: EntryRecord },
    #[serde(rename = "task")]
    Task { value: Box<TaskRecord> },
    #[serde(rename = "submission")]
    Submission { value: SubmissionRecord },
    #[serde(rename = "document.create")]
    DocumentCreate {
        record: DocumentCreate,
        content: DocumentBase,
    },
    #[serde(rename = "document.copy")]
    DocumentCopy {
        record: DocumentCreate,
        source: DocumentCopySource,
    },
    #[serde(rename = "document.change")]
    DocumentChange {
        id: DocumentId,
        content: DocumentContent,
    },
    #[serde(rename = "document.retire")]
    DocumentRetire { id: DocumentId },
}
// A create always carries a base discriminator, even though its Rust type makes
// a delta impossible. Its serializer is shared with ordinary content records.
impl Serialize for DocumentBase {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        DocumentContent::Base {
            version: self.version,
            value: self.value.clone(),
        }
        .serialize(s)
    }
}
impl<'de> Deserialize<'de> for DocumentBase {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        match DocumentContent::deserialize(d)? {
            DocumentContent::Base { version, value } => Ok(Self { version, value }),
            _ => Err(serde::de::Error::custom(
                "Document creation requires a base",
            )),
        }
    }
}
impl<'de> Deserialize<'de> for StorageWrite {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let v = JsonValue::deserialize(d)?;
        let decode = || -> std::result::Result<Self, String> {
            Ok(match wire::tag(&v, "type")?.as_str() {
                "conversation" => Self::Conversation {
                    value: wire::field(&v, "value")?,
                },
                "entry" => Self::Entry {
                    value: wire::field(&v, "value")?,
                },
                "task" => Self::Task {
                    value: wire::field(&v, "value")?,
                },
                "submission" => Self::Submission {
                    value: wire::field(&v, "value")?,
                },
                "document.create" => Self::DocumentCreate {
                    record: wire::field(&v, "record")?,
                    content: wire::field(&v, "content")?,
                },
                "document.copy" => Self::DocumentCopy {
                    record: wire::field(&v, "record")?,
                    source: wire::field(&v, "source")?,
                },
                "document.change" => Self::DocumentChange {
                    id: wire::field(&v, "id")?,
                    content: wire::field(&v, "content")?,
                },
                "document.retire" => Self::DocumentRetire {
                    id: wire::field(&v, "id")?,
                },
                other => return Err(format!("Unknown storage write {other}")),
            })
        };
        decode().map_err(serde::de::Error::custom)
    }
}

pub type Cursor = JsonObject;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Cursor>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanOrder {
    Ascending,
    Descending,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_conversation_id: Option<ConversationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<ScanOrder>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryQuery {
    pub conversation_id: ConversationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_entry_id: Option<EntryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entry_id: Option<EntryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<ScanOrder>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<ConversationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<JsonString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_requested: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<ScanOrder>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<ConversationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SubmissionStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<ScanOrder>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentQuery {
    pub scope: DocumentScope,
    pub at: DocumentPoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<JsonString>,
}
