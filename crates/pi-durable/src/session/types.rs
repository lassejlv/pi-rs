use crate::*;
use pi_chord::delta::Op;
use std::sync::Arc;

#[cfg(feature = "stock-test-probes")]
#[derive(Clone, Copy)]
pub enum TransactionProbePoint {
    Adopt(Seq),
    Discard,
}
#[cfg(feature = "stock-test-probes")]
pub(super) type TransactionProbe = Arc<dyn Fn(TransactionProbePoint) -> Result<()> + Send + Sync>;

#[derive(Clone, Copy, Debug, Default)]
pub struct TransactionScope {
    pub conversation_id: Option<ConversationId>,
    pub task_id: Option<TaskId>,
}
#[derive(Clone, Copy, Debug)]
pub enum ConversationOwnership {
    Ownerless,
    Task(TaskId),
}
#[derive(Clone, Copy, Debug)]
pub enum TaskOwnership {
    Conversation,
    Task(TaskId),
}
#[derive(Clone, Debug)]
pub enum TaskTrace {
    Root,
    Carrier(JsonObject),
}
#[derive(Clone, Debug)]
pub struct TaskOptions {
    pub ownership: TaskOwnership,
    pub conversation_id: Option<ConversationId>,
    pub trace: Option<TaskTrace>,
    pub links: Option<Vec<JsonObject>>,
    pub background: bool,
    pub abandon_on_restart: bool,
    pub submission_ids: Option<Vec<SubmissionId>>,
}
impl TaskOptions {
    pub fn new(ownership: TaskOwnership) -> Self {
        Self {
            ownership,
            conversation_id: None,
            trace: None,
            links: None,
            background: false,
            abandon_on_restart: false,
            submission_ids: None,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum EntryHead {
    Entry(EntryId),
    ThisEntry,
}
#[derive(Clone, Debug)]
pub struct EntryDraft {
    pub kind: JsonString,
    pub model: Option<Vec<JsonValue>>,
    pub data: Option<JsonValue>,
    pub head: Option<EntryHead>,
    pub edits: Option<Vec<ContextEdit>>,
}
impl EntryDraft {
    pub fn new(kind: impl Into<JsonString>) -> Self {
        Self {
            kind: kind.into(),
            model: None,
            data: None,
            head: None,
            edits: None,
        }
    }
}
impl EntryDraft {
    pub(crate) fn to_json(&self) -> JsonValue {
        use crate::wire::*;
        let mut value = object();
        put(&mut value, "kind", &self.kind);
        put_optional(&mut value, "model", &self.model);
        put_optional(&mut value, "data", &self.data);
        put_optional(&mut value, "edits", &self.edits);
        match self.head {
            Some(EntryHead::ThisEntry) => put(&mut value, "head", &"self"),
            Some(EntryHead::Entry(id)) => put(&mut value, "head", &id),
            None => {}
        }
        value
    }
    pub(crate) fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        use crate::wire::*;
        Ok(Self {
            kind: field(value, "kind")?,
            model: optional(value, "model")?,
            data: optional(value, "data")?,
            edits: optional(value, "edits")?,
            head: match get(value, "head") {
                None => None,
                Some(value) if value.to_utf8().as_deref() == Some("self") => {
                    Some(EntryHead::ThisEntry)
                }
                Some(value) => Some(EntryHead::Entry(decode(value)?)),
            },
        })
    }
}
crate::wire::raw_serde!(EntryDraft);
#[derive(Clone, Debug)]
pub struct SubmissionCreate {
    pub conversation_id: ConversationId,
    pub trace: Option<JsonObject>,
    pub request_id: Option<JsonString>,
    pub state: SubmissionState,
}
impl SubmissionCreate {
    fn to_json(&self) -> JsonValue {
        let mut value = crate::wire::encode(&self.state);
        crate::wire::put(&mut value, "conversationId", &self.conversation_id);
        crate::wire::put_optional(&mut value, "trace", &self.trace);
        crate::wire::put_optional(&mut value, "requestId", &self.request_id);
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        Ok(Self {
            conversation_id: crate::wire::field(value, "conversationId")?,
            trace: crate::wire::optional(value, "trace")?,
            request_id: crate::wire::optional(value, "requestId")?,
            state: crate::wire::decode(value)?,
        })
    }
}
crate::wire::raw_serde!(SubmissionCreate);
#[derive(Clone, Debug)]
pub enum SubmissionSettlement {
    Done {
        answer: EntryId,
    },
    Unanswered {
        reason: JsonString,
        detail: Option<JsonValue>,
    },
}
impl SubmissionSettlement {
    fn to_json(&self) -> JsonValue {
        let mut value = crate::wire::object();
        match self {
            Self::Done { answer } => {
                crate::wire::put(&mut value, "status", &"done");
                crate::wire::put(&mut value, "answer", answer);
            }
            Self::Unanswered { reason, detail } => {
                crate::wire::put(&mut value, "status", &"unanswered");
                crate::wire::put(&mut value, "reason", reason);
                crate::wire::put_optional(&mut value, "detail", detail);
            }
        }
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        Ok(match crate::wire::tag(value, "status")?.as_str() {
            "done" => Self::Done {
                answer: crate::wire::field(value, "answer")?,
            },
            "unanswered" => Self::Unanswered {
                reason: crate::wire::field(value, "reason")?,
                detail: crate::wire::optional(value, "detail")?,
            },
            status => return Err(format!("Invalid submission settlement {status}")),
        })
    }
}
crate::wire::raw_serde!(SubmissionSettlement);

/// The value shares the exact immutable revision installed during adoption.
#[derive(Clone, Debug)]
pub enum DocumentCommitChange {
    Document {
        record: DocumentRecord,
        conversation_id: Option<ConversationId>,
        version: Option<u64>,
        value: Arc<JsonValue>,
        ops: Arc<Vec<Op>>,
    },
    Copy {
        record: DocumentRecord,
        conversation_id: ConversationId,
        source: DocumentCopySource,
    },
}
impl DocumentCommitChange {
    pub fn record(&self) -> &DocumentRecord {
        match self {
            Self::Document { record, .. } | Self::Copy { record, .. } => record,
        }
    }
}
#[derive(Clone, Debug)]
pub enum CommitChange {
    Conversation(ConversationRecord),
    Entry(EntryRecord),
    Task(Box<TaskRecord>),
    Submission(SubmissionRecord),
    Document(DocumentCommitChange),
}
#[derive(Clone, Debug)]
pub struct CommitPublication {
    pub seq: Seq,
    pub changes: Vec<CommitChange>,
}
#[derive(Clone, Debug)]
pub enum SessionEnd {
    Closed,
    Failed(Error),
}

impl serde::Serialize for DocumentCommitChange {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use crate::wire::{put, put_optional};
        let mut value = crate::wire::object();
        match self {
            Self::Document {
                record,
                conversation_id,
                version,
                value: revision,
                ops,
            } => {
                put(&mut value, "type", "document");
                put(&mut value, "record", record);
                put_optional(&mut value, "conversationId", conversation_id);
                put_optional(&mut value, "version", version);
                put(&mut value, "value", revision);
                put(&mut value, "ops", ops);
            }
            Self::Copy {
                record,
                conversation_id,
                source,
            } => {
                put(&mut value, "type", "document.copy");
                put(&mut value, "record", record);
                put(&mut value, "conversationId", conversation_id);
                put(&mut value, "source", source);
            }
        }
        value.serialize(serializer)
    }
}
impl serde::Serialize for CommitChange {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let (kind, value) = match self {
            Self::Conversation(v) => ("conversation", crate::wire::encode(v)),
            Self::Entry(v) => ("entry", crate::wire::encode(v)),
            Self::Task(v) => ("task", crate::wire::encode(v)),
            Self::Submission(v) => ("submission", crate::wire::encode(v)),
            Self::Document(v) => return v.serialize(serializer),
        };
        let mut envelope = crate::wire::object();
        crate::wire::put(&mut envelope, "type", kind);
        crate::wire::put(&mut envelope, "value", &value);
        envelope.serialize(serializer)
    }
}
impl serde::Serialize for CommitPublication {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut value = crate::wire::object();
        crate::wire::put(&mut value, "seq", &self.seq);
        crate::wire::put(&mut value, "changes", &self.changes);
        value.serialize(serializer)
    }
}
