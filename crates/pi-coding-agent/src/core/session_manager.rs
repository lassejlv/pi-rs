//! Port of `packages/coding-agent/src/core/session-manager.ts`.
//!
//! Sessions are append-only trees stored as JSONL files (see `docs/session-format.md`). The
//! on-disk format is shared with the TypeScript implementation: files written by either side
//! load in the other.
//!
//! Rust-specific notes:
//! - Lines are parsed leniently. Missing `id`/`timestamp` fields (v1 sessions) default to empty
//!   strings until migration assigns them, messages whose shape does not match a known role are
//!   kept verbatim as raw custom messages, and entries of unknown `type` are preserved in
//!   [`FileEntry::Unknown`] so rewrites and forks do not drop them (they are not part of the tree).
//! - [`SessionManager`] is shared as `Arc<SessionManager>`; all methods take `&self`.
//! - Append methods return `anyhow::Result` because persisting can fail (TS throws).

use crate::config::{APP_NAME, get_agent_dir as get_default_agent_dir, get_sessions_dir};
use crate::core::messages::BashExecutionMessage;
use crate::core::messages::CustomMessage;
use crate::core::messages::{
    create_branch_summary_message, create_compaction_summary_message, create_custom_message,
    parse_timestamp_ms,
};
use crate::utils::abort::OperationAbortedError;
use crate::utils::paths::{normalize_path, resolve_path};
use futures::StreamExt;
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_agent::types::AgentMessage;
use pi_ai::types::AssistantContent;
use pi_ai::types::AssistantMessage;
use pi_ai::types::Message;
use pi_ai::types::SystemMessage;
use pi_ai::types::ToolResultMessage;
use pi_ai::types::Usage;
use pi_ai::types::UserContent;
use pi_ai::types::UserMessage;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read, Write};
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const CURRENT_SESSION_VERSION: i64 = 3;

/// Session file header (`{"type":"session", ...}`, always the first line).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    /// v1 sessions don't have this
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
    pub id: String,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntryBase {
    pub r#type: String,
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub timestamp: String,
}

/// Deserialize a stored message without validation, like TS `JSON.parse`: messages whose fields
/// do not match their role are kept as raw custom messages so they round-trip unchanged.
fn lenient_agent_message<'de, D: Deserializer<'de>>(deserializer: D) -> Result<AgentMessage, D::Error> {
    let value = Value::deserialize(deserializer)?;
    match AgentMessage::from_value(value.clone()) {
        Ok(message) => Ok(message),
        Err(error) => match value {
            Value::Object(mut map) => {
                let role = match map.shift_remove("role") {
                    Some(Value::String(role)) => role,
                    _ => String::new(),
                };
                Ok(AgentMessage::Custom(pi_agent::types::CustomMessage { role, fields: map }))
            }
            _ => Err(serde::de::Error::custom(error)),
        },
    }
}

// TS: extends SessionEntryBase (inherited fields inlined)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessageEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    #[serde(deserialize_with = "lenient_agent_message")]
    pub message: AgentMessage,
    /// Fields this version does not know, kept so rewriting the file preserves them (TS writes
    /// entries back exactly as parsed).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// TS: extends SessionEntryBase (inherited fields inlined)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingLevelChangeEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    pub thinking_level: String,
    /// Fields this version does not know, kept so rewriting the file preserves them (TS writes
    /// entries back exactly as parsed).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// TS: extends SessionEntryBase (inherited fields inlined)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChangeEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    pub provider: String,
    pub model_id: String,
    /// Fields this version does not know, kept so rewriting the file preserves them (TS writes
    /// entries back exactly as parsed).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// TS: extends SessionEntryBase (inherited fields inlined)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    /// Arbitrary usage category, such as "cache_warm".
    pub kind: String,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    /// Optional human-readable qualifier for usage notices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Fields this version does not know, kept so rewriting the file preserves them (TS writes
    /// entries back exactly as parsed).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Prefix marking a v1 `firstKeptEntryIndex` until migration resolves it to an entry id.
const V1_FIRST_KEPT_INDEX_PREFIX: &str = "\u{0}firstKeptEntryIndex:";

// TS: extends SessionEntryBase (inherited fields inlined)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "CompactionEntryWire")]
pub struct CompactionEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    /// Extension-specific data (e.g., ArtifactIndex, version markers for structured compaction)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Usage from the LLM call(s) that generated this summary, if available
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// True if generated by an extension, undefined/false if pi-generated (backward compatible)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
    /// Complete prompt and tool state at this compaction boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message: Option<SystemMessage>,
    /// Fields this version does not know, kept so rewriting the file preserves them (TS writes
    /// entries back exactly as parsed).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Deserialization shape of [`CompactionEntry`], accepting v1 `firstKeptEntryIndex`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompactionEntryWire {
    #[serde(default)]
    id: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    first_kept_entry_id: Option<String>,
    #[serde(default)]
    first_kept_entry_index: Option<Value>,
    #[serde(default, with = "pi_ai::serde_util::lax_u64")]
    tokens_before: u64,
    #[serde(default, deserialize_with = "pi_ai::serde_util::deserialize_some")]
    details: Option<Value>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    from_hook: Option<bool>,
    #[serde(default)]
    system_message: Option<SystemMessage>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl From<CompactionEntryWire> for CompactionEntry {
    fn from(wire: CompactionEntryWire) -> Self {
        let first_kept_entry_id = match (wire.first_kept_entry_id, wire.first_kept_entry_index) {
            (None, Some(Value::Number(index))) if index.as_f64().is_some() => {
                format!("{V1_FIRST_KEPT_INDEX_PREFIX}{}", index.as_f64().unwrap_or(-1.0))
            }
            (Some(id), _) => id,
            (None, _) => String::new(),
        };
        Self {
            id: wire.id,
            parent_id: wire.parent_id,
            timestamp: wire.timestamp,
            summary: wire.summary,
            first_kept_entry_id,
            tokens_before: wire.tokens_before,
            details: wire.details,
            usage: wire.usage,
            from_hook: wire.from_hook,
            system_message: wire.system_message,
            extra: wire.extra,
        }
    }
}

// TS: extends SessionEntryBase (inherited fields inlined)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    pub from_id: String,
    pub summary: String,
    /// Extension-specific data (not sent to LLM)
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "pi_ai::serde_util::deserialize_some"
    )]
    pub details: Option<Value>,
    /// Usage from the LLM call that generated this summary, if available
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// True if generated by an extension, false if pi-generated
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
    /// Fields this version does not know, kept so rewriting the file preserves them (TS writes
    /// entries back exactly as parsed).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Custom entry for extensions to store extension-specific data in the session.
/// Use customType to identify your extension's entries.
///
/// Purpose: Persist extension state across session reloads. On reload, extensions can
/// scan entries for their customType and reconstruct internal state.
///
/// Does NOT participate in LLM context (ignored by buildSessionContext).
/// For injecting content into context, see CustomMessageEntry.
// TS: extends SessionEntryBase (inherited fields inlined). Field order matches the TS writer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEntry {
    pub custom_type: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "pi_ai::serde_util::deserialize_some"
    )]
    pub data: Option<Value>,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    /// Fields this version does not know, kept so rewriting the file preserves them (TS writes
    /// entries back exactly as parsed).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Label entry for user-defined bookmarks/markers on entries.
// TS: extends SessionEntryBase (inherited fields inlined)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Fields this version does not know, kept so rewriting the file preserves them (TS writes
    /// entries back exactly as parsed).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Session metadata entry (e.g., user-defined display name).
// TS: extends SessionEntryBase (inherited fields inlined)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Fields this version does not know, kept so rewriting the file preserves them (TS writes
    /// entries back exactly as parsed).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Custom message entry for extensions to inject messages into LLM context.
/// Use customType to identify your extension's entries.
///
/// Unlike CustomEntry, this DOES participate in LLM context.
/// The content is converted to a user message in buildSessionContext().
/// Use details for extension-specific metadata (not sent to LLM).
///
/// display controls TUI rendering:
/// - false: hidden entirely
/// - true: rendered with distinct styling (different from user messages)
// TS: extends SessionEntryBase (inherited fields inlined). Field order matches the TS writer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessageEntry {
    pub custom_type: String,
    #[serde(default, deserialize_with = "pi_ai::serde_util::null_as_default")]
    pub content: UserContent,
    #[serde(default)]
    pub display: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "pi_ai::serde_util::deserialize_some"
    )]
    pub details: Option<Value>,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    /// Fields this version does not know, kept so rewriting the file preserves them (TS writes
    /// entries back exactly as parsed).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Content that an append-only context edit may replace without changing message metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContextEditableContent {
    /// User, tool-result or custom message content (`string | (TextContent | ImageContent)[]`).
    User(UserContent),
    /// Assistant message content (text, thinking and tool call blocks).
    Assistant(Vec<AssistantContent>),
}

impl From<UserContent> for ContextEditableContent {
    fn from(value: UserContent) -> Self {
        Self::User(value)
    }
}

impl From<&str> for ContextEditableContent {
    fn from(value: &str) -> Self {
        Self::User(UserContent::Text(value.to_string()))
    }
}

impl From<String> for ContextEditableContent {
    fn from(value: String) -> Self {
        Self::User(UserContent::Text(value))
    }
}

impl From<Vec<AssistantContent>> for ContextEditableContent {
    fn from(value: Vec<AssistantContent>) -> Self {
        Self::Assistant(value)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEditEntryReplacement {
    pub content: ContextEditableContent,
}

/// Append-only change to one earlier entry's contribution to model context.
// TS: extends SessionEntryBase (inherited fields inlined)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEditEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    pub target_id: String,
    /// Null omits the target from model context. A value replaces only its content.
    #[serde(default)]
    pub replacement: Option<ContextEditEntryReplacement>,
    /// Fields this version does not know, kept so rewriting the file preserves them (TS writes
    /// entries back exactly as parsed).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Session entry - has id/parentId for tree structure (returned by "read" methods in SessionManager)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum SessionEntry {
    #[serde(rename = "message")]
    Message(SessionMessageEntry),
    #[serde(rename = "thinking_level_change")]
    ThinkingLevelChange(ThinkingLevelChangeEntry),
    #[serde(rename = "model_change")]
    ModelChange(ModelChangeEntry),
    #[serde(rename = "usage")]
    Usage(UsageEntry),
    #[serde(rename = "compaction")]
    Compaction(CompactionEntry),
    #[serde(rename = "branch_summary")]
    BranchSummary(BranchSummaryEntry),
    #[serde(rename = "custom")]
    Custom(CustomEntry),
    #[serde(rename = "custom_message")]
    CustomMessage(CustomMessageEntry),
    #[serde(rename = "context_edit")]
    ContextEdit(ContextEditEntry),
    #[serde(rename = "label")]
    Label(LabelEntry),
    #[serde(rename = "session_info")]
    SessionInfo(SessionInfoEntry),
    /// An entry whose `type` this version does not know (or whose fields do not match it). Like TS,
    /// it is part of the tree and written back verbatim; context building ignores it.
    #[serde(untagged)]
    Unknown(UnknownSessionEntry),
}

/// A tree entry of unknown type, kept as its raw JSON object.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownSessionEntry {
    /// The `type` field.
    pub entry_type: String,
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    /// The complete JSON object as read (and as written back).
    pub raw: Map<String, Value>,
}

impl UnknownSessionEntry {
    /// Wrap a raw entry object. Requires string `type` (other than `session`) and `id` fields.
    pub fn from_map(raw: Map<String, Value>) -> Option<Self> {
        let entry_type = raw.get("type")?.as_str()?.to_string();
        if entry_type == "session" {
            return None;
        }
        let id = raw.get("id")?.as_str()?.to_string();
        let parent_id = raw.get("parentId").and_then(Value::as_str).map(str::to_string);
        let timestamp = raw.get("timestamp").and_then(Value::as_str).unwrap_or_default().to_string();
        Some(Self { entry_type, id, parent_id, timestamp, raw })
    }

    fn set_id(&mut self, id: String) {
        self.raw.insert("id".into(), Value::String(id.clone()));
        self.id = id;
    }

    fn set_parent_id(&mut self, parent_id: Option<String>) {
        self.raw.insert("parentId".into(), parent_id.clone().map(Value::String).unwrap_or(Value::Null));
        self.parent_id = parent_id;
    }
}

impl Serialize for UnknownSessionEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for UnknownSessionEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Map::<String, Value>::deserialize(deserializer)?;
        UnknownSessionEntry::from_map(raw)
            .ok_or_else(|| serde::de::Error::custom("session entry needs string `type` and `id` fields"))
    }
}

macro_rules! each_entry {
    ($self:expr, $e:ident => $body:expr) => {
        match $self {
            SessionEntry::Message($e) => $body,
            SessionEntry::ThinkingLevelChange($e) => $body,
            SessionEntry::ModelChange($e) => $body,
            SessionEntry::Usage($e) => $body,
            SessionEntry::Compaction($e) => $body,
            SessionEntry::BranchSummary($e) => $body,
            SessionEntry::Custom($e) => $body,
            SessionEntry::CustomMessage($e) => $body,
            SessionEntry::ContextEdit($e) => $body,
            SessionEntry::Label($e) => $body,
            SessionEntry::SessionInfo($e) => $body,
            SessionEntry::Unknown($e) => $body,
        }
    };
}

impl SessionEntry {
    /// The `type` discriminant (e.g. `"message"`).
    pub fn type_name(&self) -> &str {
        match self {
            SessionEntry::Message(_) => "message",
            SessionEntry::ThinkingLevelChange(_) => "thinking_level_change",
            SessionEntry::ModelChange(_) => "model_change",
            SessionEntry::Usage(_) => "usage",
            SessionEntry::Compaction(_) => "compaction",
            SessionEntry::BranchSummary(_) => "branch_summary",
            SessionEntry::Custom(_) => "custom",
            SessionEntry::CustomMessage(_) => "custom_message",
            SessionEntry::ContextEdit(_) => "context_edit",
            SessionEntry::Label(_) => "label",
            SessionEntry::SessionInfo(_) => "session_info",
            SessionEntry::Unknown(e) => &e.entry_type,
        }
    }

    pub fn id(&self) -> &str {
        each_entry!(self, e => &e.id)
    }

    pub fn parent_id(&self) -> Option<&str> {
        each_entry!(self, e => e.parent_id.as_deref())
    }

    pub fn timestamp(&self) -> &str {
        each_entry!(self, e => &e.timestamp)
    }

    pub fn set_id(&mut self, id: String) {
        match self {
            SessionEntry::Unknown(e) => e.set_id(id),
            other => each_entry!(other, e => e.id = id),
        }
    }

    pub fn set_parent_id(&mut self, parent_id: Option<String>) {
        match self {
            SessionEntry::Unknown(e) => e.set_parent_id(parent_id),
            other => each_entry!(other, e => e.parent_id = parent_id),
        }
    }

    /// The message of a `message` entry.
    pub fn message(&self) -> Option<&AgentMessage> {
        match self {
            SessionEntry::Message(e) => Some(&e.message),
            _ => None,
        }
    }

    /// The JSON object written to the session file.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Raw file entry (includes header)
#[derive(Debug, Clone, PartialEq)]
// Mirrors a TS union; boxing the large variants would add indirection to every
// construction and match across the crate for little memory saved.
#[allow(clippy::large_enum_variant)]
pub enum FileEntry {
    /// `{"type":"session", ...}` header.
    Session(SessionHeader),
    /// A tree entry.
    Entry(SessionEntry),
    /// A JSON object whose `type` this version does not know (or whose fields do not match it).
    /// Preserved verbatim when the file is rewritten or forked; not part of the session tree.
    Unknown(Map<String, Value>),
}

impl FileEntry {
    pub fn type_name(&self) -> &str {
        match self {
            FileEntry::Session(_) => "session",
            FileEntry::Entry(e) => e.type_name(),
            FileEntry::Unknown(map) => map.get("type").and_then(Value::as_str).unwrap_or(""),
        }
    }

    pub fn as_entry(&self) -> Option<&SessionEntry> {
        match self {
            FileEntry::Entry(e) => Some(e),
            _ => None,
        }
    }

    pub fn as_header(&self) -> Option<&SessionHeader> {
        match self {
            FileEntry::Session(h) => Some(h),
            _ => None,
        }
    }

    /// Parse one JSON value like a session file line. Non-objects are rejected.
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        let Value::Object(map) = value else {
            return Err(serde::de::Error::custom("session entry must be an object"));
        };
        let is_header = map.get("type").and_then(Value::as_str) == Some("session");
        if is_header {
            if let Ok(header) = SessionHeader::deserialize(&Value::Object(map.clone())) {
                return Ok(FileEntry::Session(header));
            }
            return Ok(FileEntry::Unknown(map));
        }
        match SessionEntry::deserialize(&Value::Object(map.clone())) {
            Ok(entry) => Ok(FileEntry::Entry(entry)),
            Err(_) => Ok(FileEntry::Unknown(map)),
        }
    }
}

impl From<SessionEntry> for FileEntry {
    fn from(value: SessionEntry) -> Self {
        FileEntry::Entry(value)
    }
}

impl From<SessionHeader> for FileEntry {
    fn from(value: SessionHeader) -> Self {
        FileEntry::Session(value)
    }
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum HeaderRef<'a> {
    #[serde(rename = "session")]
    Session(&'a SessionHeader),
}


impl Serialize for FileEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            FileEntry::Session(header) => HeaderRef::Session(header).serialize(serializer),
            FileEntry::Entry(entry) => entry.serialize(serializer),
            FileEntry::Unknown(map) => map.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for FileEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        FileEntry::from_value(value).map_err(serde::de::Error::custom)
    }
}

/// Tree node for getTree() - defensive copy of session structure
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTreeNode {
    pub entry: SessionEntry,
    pub children: Vec<SessionTreeNode>,
    /// Resolved label for this entry, if any
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Timestamp of the latest label change for this entry, if any
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_timestamp: Option<String>,
}

impl Drop for SessionTreeNode {
    /// Drop iteratively: long linear sessions produce trees thousands of levels deep.
    fn drop(&mut self) {
        let mut stack = std::mem::take(&mut self.children);
        while let Some(mut node) = stack.pop() {
            stack.append(&mut node.children);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedSessionEntry {
    /// Raw append-only entry that owns this projected contribution.
    pub source_entry: SessionEntry,
    /// Model-visible messages after context edits. Empty for state-only entries and omissions.
    pub messages: Vec<AgentMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionProjectionModel {
    pub provider: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionProjection {
    pub entries: Vec<ProjectedSessionEntry>,
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    #[serde(default)]
    pub model: Option<SessionProjectionModel>,
}

pub type SessionContextModel = SessionProjectionModel;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    #[serde(default)]
    pub model: Option<SessionContextModel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub path: String,
    pub id: String,
    /// Working directory where the session was started. Empty string for old sessions.
    pub cwd: String,
    /// User-defined display name from session_info entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Path to the parent session (if this session was forked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_path: Option<String>,
    pub created: chrono::DateTime<chrono::Utc>,
    pub modified: chrono::DateTime<chrono::Utc>,
    pub message_count: i64,
    pub first_message: String,
    pub all_messages_text: String,
}

/// TS: Pick< SessionManager, | "getCwd" | "getSessionDir" | "getSessionId" | "getSessionFile" | "getLeafId" | "getLeafEntry" | "getEntry" | "getLabel" | "getBranch" | "buildContextEntries" | "buildSessionProjection" | "getHeader" | "getEntries" | "getTree" | "getSessionName" >
pub type ReadonlySessionManager = Arc<SessionManager>;

// ============================================================================
// Helpers
// ============================================================================

fn create_session_id() -> String {
    pi_ai::uuidv7()
}

/// `new Date().toISOString()`.
pub(crate) fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// `new Date(ts).getTime()`, `None` for invalid dates.
fn parse_iso_ms(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|dt| dt.timestamp_millis())
        .ok()
}

fn ms_to_datetime(ms: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default()
}

/// `timestamp.replace(/[:.]/g, "-")`.
fn file_timestamp(timestamp: &str) -> String {
    timestamp.replace([':', '.'], "-")
}

fn join_path(dir: &str, name: &str) -> String {
    Path::new(dir).join(name).to_string_lossy().into_owned()
}

fn resolve(path: &str) -> String {
    resolve_path(path, None, None)
}

fn exists(path: &str) -> bool {
    !path.is_empty() && Path::new(path).exists()
}

fn current_dir() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string())
}

fn to_json_line<T: Serialize>(value: &T) -> String {
    let mut line = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    line.push('\n');
    line
}

fn append_to_file(path: &str, content: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(content.as_bytes())
}

pub fn assert_valid_session_id(id: &str) -> anyhow::Result<()> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$").expect("valid regex")
    });
    if !RE.is_match(id) {
        anyhow::bail!(
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character"
        );
    }
    Ok(())
}

/// Generate a unique short ID (8 hex chars, collision-checked)
fn generate_id(has: impl Fn(&str) -> bool) -> String {
    for _ in 0..100 {
        let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        if !has(&id) {
            return id;
        }
    }
    // Fallback to full UUID if somehow we have collisions
    uuid::Uuid::new_v4().to_string()
}

/// Migrate v1 → v2: add id/parentId tree structure. Mutates in place.
fn migrate_v1_to_v2(entries: &mut [FileEntry]) {
    let ids: HashSet<String> = HashSet::new();
    let mut prev_id: Option<String> = None;
    // Ids assigned so far, by file position (v1 compaction entries refer to positions).
    let mut assigned: Vec<Option<String>> = vec![None; entries.len()];

    for index in 0..entries.len() {
        match &mut entries[index] {
            FileEntry::Session(header) => {
                header.version = Some(2);
                continue;
            }
            FileEntry::Unknown(map) => {
                // TS assigns ids to every non-header entry.
                let id = generate_id(|candidate| ids.contains(candidate));
                map.insert("id".into(), Value::String(id.clone()));
                map.insert(
                    "parentId".into(),
                    prev_id.clone().map(Value::String).unwrap_or(Value::Null),
                );
                assigned[index] = Some(id.clone());
                prev_id = Some(id);
                // With an id it is a tree entry, as in TS.
                if let Some(entry) = UnknownSessionEntry::from_map(map.clone()) {
                    entries[index] = FileEntry::Entry(SessionEntry::Unknown(entry));
                }
            }
            FileEntry::Entry(entry) => {
                let id = generate_id(|candidate| ids.contains(candidate));
                entry.set_id(id.clone());
                entry.set_parent_id(prev_id.clone());
                assigned[index] = Some(id.clone());
                prev_id = Some(id);
            }
        }

        // Convert firstKeptEntryIndex to firstKeptEntryId for compaction
        let target_index = match &entries[index] {
            FileEntry::Entry(SessionEntry::Compaction(comp)) => comp
                .first_kept_entry_id
                .strip_prefix(V1_FIRST_KEPT_INDEX_PREFIX)
                .map(|raw| raw.parse::<f64>().unwrap_or(-1.0)),
            _ => None,
        };
        if let Some(target_index) = target_index {
            let target_id = if target_index >= 0.0 && target_index.fract() == 0.0 {
                let target_index = target_index as usize;
                match entries.get(target_index) {
                    Some(FileEntry::Session(_)) | None => None,
                    Some(FileEntry::Entry(target)) => Some(
                        assigned
                            .get(target_index)
                            .cloned()
                            .flatten()
                            .unwrap_or_else(|| target.id().to_string()),
                    ),
                    Some(FileEntry::Unknown(map)) => assigned
                        .get(target_index)
                        .cloned()
                        .flatten()
                        .or_else(|| map.get("id").and_then(Value::as_str).map(str::to_string)),
                }
            } else {
                None
            };
            if let FileEntry::Entry(SessionEntry::Compaction(comp)) = &mut entries[index] {
                comp.first_kept_entry_id = target_id.unwrap_or_default();
            }
        }
    }
}

/// Migrate v2 → v3: rename hookMessage role to custom. Mutates in place.
fn migrate_v2_to_v3(entries: &mut [FileEntry]) {
    for entry in entries.iter_mut() {
        match entry {
            FileEntry::Session(header) => {
                header.version = Some(3);
            }
            FileEntry::Entry(SessionEntry::Message(msg_entry)) => {
                if let AgentMessage::Custom(custom) = &mut msg_entry.message
                    && custom.role == "hookMessage"
                {
                    custom.role = "custom".to_string();
                }
            }
            FileEntry::Unknown(map) if map.get("type").and_then(Value::as_str) == Some("message") => {
                if let Some(Value::Object(message)) = map.get_mut("message")
                    && message.get("role").and_then(Value::as_str) == Some("hookMessage")
                {
                    message.insert("role".into(), Value::String("custom".into()));
                }
            }
            FileEntry::Entry(SessionEntry::Unknown(unknown)) if unknown.entry_type == "message" => {
                if let Some(Value::Object(message)) = unknown.raw.get_mut("message")
                    && message.get("role").and_then(Value::as_str) == Some("hookMessage")
                {
                    message.insert("role".into(), Value::String("custom".into()));
                }
            }
            _ => {}
        }
    }
}

/// Run all necessary migrations to bring entries to current version.
/// Mutates entries in place. Returns true if any migration was applied.
fn migrate_to_current_version(entries: &mut [FileEntry]) -> bool {
    let version = entries
        .iter()
        .find_map(FileEntry::as_header)
        .and_then(|header| header.version)
        .unwrap_or(1);

    if version >= CURRENT_SESSION_VERSION {
        return false;
    }

    if version < 2 {
        migrate_v1_to_v2(entries);
    }
    if version < 3 {
        migrate_v2_to_v3(entries);
    }

    true
}

/// Exported for testing
pub fn migrate_session_entries(entries: &mut [FileEntry]) {
    migrate_to_current_version(entries);
}

/// Parse one JSONL line. Blank and malformed lines yield `None`.
fn parse_session_entry_line(line: &str) -> Option<Value> {
    if line.trim().is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(line).ok()
}

/// Exported for compaction.test.ts
pub fn parse_session_entries(content: &str) -> Vec<FileEntry> {
    content
        .trim()
        .split('\n')
        .filter_map(parse_session_entry_line)
        .filter_map(|value| FileEntry::from_value(value).ok())
        .collect()
}

pub fn get_latest_compaction_entry(entries: &[SessionEntry]) -> Option<CompactionEntry> {
    entries.iter().rev().find_map(|entry| match entry {
        SessionEntry::Compaction(c) => Some(c.clone()),
        _ => None,
    })
}

type Lookup<'a> = dyn Fn(&str) -> Option<&'a SessionEntry> + 'a;

fn build_session_path_with<'a>(
    last: Option<&'a SessionEntry>,
    leaf_id: Option<Option<&str>>,
    lookup: &Lookup<'a>,
) -> Vec<&'a SessionEntry> {
    let mut leaf: Option<&'a SessionEntry> = None;
    match leaf_id {
        // Explicit null leaf: before any entries.
        Some(None) => return Vec::new(),
        Some(Some(id)) if !id.is_empty() => leaf = lookup(id),
        _ => {}
    }
    let Some(leaf) = leaf.or(last) else {
        return Vec::new();
    };

    let mut path = Vec::new();
    let mut current = Some(leaf);
    let mut guard = 0usize;
    while let Some(entry) = current {
        path.push(entry);
        current = match entry.parent_id() {
            Some(parent) if !parent.is_empty() => lookup(parent),
            _ => None,
        };
        guard += 1;
        // Defensive: a corrupt file with a parent cycle would loop forever (TS hangs).
        if guard > 10_000_000 {
            break;
        }
    }
    path.reverse();
    path
}

fn index_entries(entries: &[SessionEntry]) -> HashMap<&str, &SessionEntry> {
    let mut index = HashMap::with_capacity(entries.len());
    for entry in entries {
        index.insert(entry.id(), entry);
    }
    index
}

fn with_lookup<'a, R>(
    entries: &'a [SessionEntry],
    by_id: Option<&'a IndexMap<String, SessionEntry>>,
    f: impl FnOnce(&Lookup<'a>) -> R,
) -> R {
    match by_id {
        Some(map) => f(&|id: &str| map.get(id)),
        None => {
            let index = index_entries(entries);
            f(&move |id: &str| index.get(id).copied())
        }
    }
}

fn get_session_context_settings(path: &[&SessionEntry]) -> (String, Option<SessionProjectionModel>) {
    let mut thinking_level = "off".to_string();
    let mut model = None;
    for entry in path {
        match entry {
            SessionEntry::ThinkingLevelChange(e) => thinking_level = e.thinking_level.clone(),
            SessionEntry::ModelChange(e) => {
                model = Some(SessionProjectionModel {
                    provider: e.provider.clone(),
                    model_id: e.model_id.clone(),
                })
            }
            SessionEntry::Message(e) => {
                if let AgentMessage::Assistant(message) = &e.message {
                    model = Some(SessionProjectionModel {
                        provider: message.provider.clone(),
                        model_id: message.model.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    (thinking_level, model)
}

/// Project one selected session entry into LLM/runtime messages.
/// Plain custom entries are display/state entries and do not participate in context.
pub fn session_entry_to_context_messages(entry: &SessionEntry) -> Vec<AgentMessage> {
    match entry {
        // Session files are parsed leniently, so null/missing content already reads as empty.
        SessionEntry::Message(e) => vec![e.message.clone()],
        SessionEntry::CustomMessage(e) => vec![
            create_custom_message(
                &e.custom_type,
                e.content.clone(),
                e.display,
                e.details.clone(),
                &e.timestamp,
            )
            .into(),
        ],
        SessionEntry::BranchSummary(e) if !e.summary.is_empty() => {
            vec![create_branch_summary_message(&e.summary, &e.from_id, &e.timestamp).into()]
        }
        SessionEntry::Compaction(e) => {
            let summary: AgentMessage =
                create_compaction_summary_message(&e.summary, e.tokens_before, &e.timestamp).into();
            match &e.system_message {
                Some(system) => vec![AgentMessage::System(system.clone()), summary],
                None => vec![summary],
            }
        }
        _ => Vec::new(),
    }
}

fn build_context_entries_from_path<'a>(path: Vec<&'a SessionEntry>) -> Vec<&'a SessionEntry> {
    let compaction = path.iter().rev().find_map(|entry| match entry {
        SessionEntry::Compaction(c) => Some(c),
        _ => None,
    });
    let Some(compaction) = compaction else {
        return path;
    };

    let Some(compaction_idx) = path.iter().position(|entry| entry.id() == compaction.id) else {
        return path;
    };

    let mut context_entries: Vec<&'a SessionEntry> = vec![path[compaction_idx]];
    let mut found_first_kept = false;
    for entry in &path[..compaction_idx] {
        if entry.id() == compaction.first_kept_entry_id {
            found_first_kept = true;
        }
        let is_system_message = matches!(entry, SessionEntry::Message(m) if m.message.role() == "system");
        if found_first_kept && !is_system_message {
            context_entries.push(entry);
        }
    }
    context_entries.extend_from_slice(&path[compaction_idx + 1..]);
    context_entries
}

/// Build the active, compaction-aware session entry list.
///
/// This follows the current leaf path. If the path contains compaction entries,
/// the latest compaction is represented by the compaction entry itself, followed
/// by the kept entries starting at firstKeptEntryId and all entries after the
/// compaction entry. Older summarized entries are omitted.
///
/// `leaf_id`: `None` = last entry (TS `undefined`), `Some(None)` = before any entry (TS `null`).
pub fn build_context_entries(
    entries: &[SessionEntry],
    leaf_id: Option<Option<&str>>,
    by_id: Option<&IndexMap<String, SessionEntry>>,
) -> Vec<SessionEntry> {
    with_lookup(entries, by_id, |lookup| {
        let path = build_session_path_with(entries.last(), leaf_id, lookup);
        build_context_entries_from_path(path).into_iter().cloned().collect()
    })
}

fn replacement_content_value(content: &ContextEditableContent, wrap_string: bool) -> Value {
    match content {
        ContextEditableContent::User(UserContent::Text(text)) if wrap_string => {
            serde_json::json!([{ "type": "text", "text": text }])
        }
        other => serde_json::to_value(other).unwrap_or(Value::Null),
    }
}

/// Apply a context edit's replacement to one projected message.
fn project_message(message: AgentMessage, replacement: &ContextEditEntryReplacement) -> AgentMessage {
    let role = message.role().to_string();
    if !matches!(role.as_str(), "user" | "assistant" | "toolResult" | "custom") {
        return message;
    }
    let wrap_string = role == "assistant" || role == "toolResult";
    let content = replacement_content_value(&replacement.content, wrap_string);
    let mut value = message.to_value();
    if let Value::Object(map) = &mut value {
        map.insert("content".into(), content);
    }
    match AgentMessage::from_value(value.clone()) {
        Ok(message) => message,
        Err(_) => match value {
            Value::Object(mut map) => {
                map.shift_remove("role");
                AgentMessage::Custom(pi_agent::types::CustomMessage { role, fields: map })
            }
            _ => message,
        },
    }
}

fn project_context_entry(entry: &SessionEntry, edit: Option<&ContextEditEntry>) -> Vec<AgentMessage> {
    let messages = session_entry_to_context_messages(entry);
    let Some(edit) = edit else {
        return messages;
    };
    let Some(replacement) = &edit.replacement else {
        return Vec::new();
    };
    messages
        .into_iter()
        .map(|message| project_message(message, replacement))
        .collect()
}

fn build_projection_from_path(path: Vec<&SessionEntry>) -> SessionProjection {
    let (thinking_level, model) = get_session_context_settings(&path);
    let context_entries = build_context_entries_from_path(path);
    let mut edits: HashMap<&str, &ContextEditEntry> = HashMap::new();
    for entry in &context_entries {
        if let SessionEntry::ContextEdit(edit) = entry {
            edits.insert(edit.target_id.as_str(), edit);
        }
    }
    let mut messages = Vec::new();
    let projected: Vec<ProjectedSessionEntry> = context_entries
        .iter()
        .enumerate()
        .map(|(index, source_entry)| {
            // buildContextEntries() may retain an older compaction entry because its
            // raw ID lies inside the newest retained range. Only the newest compaction
            // at index zero contributes a checkpoint and summary.
            let entry_messages = if matches!(source_entry, SessionEntry::Compaction(_)) && index > 0 {
                Vec::new()
            } else {
                project_context_entry(source_entry, edits.get(source_entry.id()).copied())
            };
            messages.extend(entry_messages.iter().cloned());
            ProjectedSessionEntry {
                source_entry: (*source_entry).clone(),
                messages: entry_messages,
            }
        })
        .collect();
    SessionProjection {
        entries: projected,
        messages,
        thinking_level,
        model,
    }
}

/// Build provenance-preserving, compaction-aware model context.
pub fn build_session_projection(
    entries: &[SessionEntry],
    leaf_id: Option<Option<&str>>,
    by_id: Option<&IndexMap<String, SessionEntry>>,
) -> SessionProjection {
    with_lookup(entries, by_id, |lookup| {
        let path = build_session_path_with(entries.last(), leaf_id, lookup);
        build_projection_from_path(path)
    })
}

/// Build the finalized model context from the canonical session projection.
pub fn build_session_context(
    entries: &[SessionEntry],
    leaf_id: Option<Option<&str>>,
    by_id: Option<&IndexMap<String, SessionEntry>>,
) -> SessionContext {
    let projection = build_session_projection(entries, leaf_id, by_id);
    SessionContext {
        messages: projection.messages,
        thinking_level: projection.thinking_level,
        model: projection.model,
    }
}

/// Compute the default session directory for a cwd.
/// Encodes cwd into a safe directory name under ~/.pi/agent/sessions/.
fn get_default_session_dir_path(cwd: &str, agent_dir: Option<&str>) -> String {
    let resolved_cwd = resolve(cwd);
    let resolved_agent_dir = resolve(&agent_dir.map(str::to_string).unwrap_or_else(get_default_agent_dir));
    let trimmed = resolved_cwd
        .strip_prefix('/')
        .or_else(|| resolved_cwd.strip_prefix('\\'))
        .unwrap_or(&resolved_cwd);
    let safe_path = format!("--{}--", trimmed.replace(['/', '\\', ':'], "-"));
    Path::new(&resolved_agent_dir)
        .join("sessions")
        .join(safe_path)
        .to_string_lossy()
        .into_owned()
}

pub fn get_default_session_dir(cwd: &str, agent_dir: Option<&str>) -> anyhow::Result<String> {
    let session_dir = get_default_session_dir_path(cwd, agent_dir);
    if !exists(&session_dir) {
        std::fs::create_dir_all(&session_dir)?;
    }
    Ok(session_dir)
}

/// Bound synchronous header discovery while allowing large cwd and custom metadata fields.
const MAX_SESSION_HEADER_SCAN_BYTES: u64 = 1024 * 1024;

/// TS: `extends Error`
#[derive(Debug, Clone)]
pub struct SessionHeaderScanLimitError {
    pub message: String,
}

impl std::fmt::Display for SessionHeaderScanLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SessionHeaderScanLimitError {}

impl SessionHeaderScanLimitError {
    pub fn new(file_path: &str) -> Self {
        Self {
            message: format!(
                "Session header exceeds {MAX_SESSION_HEADER_SCAN_BYTES}-byte scan limit: {file_path}"
            ),
        }
    }
}

/// Read and parse every line of a session file. Returns the parsed values and whether the file
/// ends with a partial (unterminated) line.
fn read_session_values(path: &str) -> std::io::Result<(Vec<Value>, bool)> {
    // Stream line by line: session files can be larger than what fits comfortably in one string.
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(1024 * 1024, file);
    let mut values = Vec::new();
    let mut line = Vec::new();
    let mut has_partial_line = false;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            has_partial_line = false;
        } else {
            has_partial_line = !line.is_empty();
        }
        if let Some(value) = parse_session_entry_line(&String::from_utf8_lossy(&line)) {
            values.push(value);
        }
    }
    Ok((values, has_partial_line))
}

fn is_header_value(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("session")
        && value.get("id").is_some_and(Value::is_string)
}

/// Exported for testing
pub fn load_entries_from_file(file_path: &str) -> anyhow::Result<Vec<FileEntry>> {
    let resolved_file_path = normalize_path(file_path, None);
    if !exists(&resolved_file_path) {
        return Ok(Vec::new());
    }

    let (values, has_partial_line) = read_session_values(&resolved_file_path)?;

    // Validate session header before repairing the file.
    let Some(first) = values.first() else {
        return Ok(Vec::new());
    };
    if !is_header_value(first) {
        return Ok(Vec::new());
    }

    if has_partial_line {
        append_to_file(&resolved_file_path, "\n")?;
    }
    Ok(values
        .into_iter()
        .filter_map(|value| FileEntry::from_value(value).ok())
        .collect())
}

/// Inspect a physical line while searching for the first parsed session entry.
/// Blank and malformed lines are skipped to match loadEntriesFromFile().
/// Returns None to keep scanning, Some(None) for a parsed non-header entry, or the header.
fn parse_session_header_candidate(line: &[u8]) -> Option<Option<SessionHeader>> {
    let line = String::from_utf8_lossy(line);
    let value = parse_session_entry_line(&line)?;
    if !is_header_value(&value) {
        return Some(None);
    }
    Some(SessionHeader::deserialize(&value).ok())
}

fn read_session_header(file_path: &str) -> anyhow::Result<Option<SessionHeader>> {
    let file = std::fs::File::open(file_path)?;
    let mut reader = std::io::BufReader::new(file.take(MAX_SESSION_HEADER_SCAN_BYTES + 1));
    let mut scanned: u64 = 0;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        scanned += read as u64;
        if scanned > MAX_SESSION_HEADER_SCAN_BYTES {
            return Err(SessionHeaderScanLimitError::new(file_path).into());
        }
        if read == 0 {
            return Ok(None);
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            if let Some(header) = parse_session_header_candidate(&line) {
                return Ok(header);
            }
        } else {
            // Final line without a newline.
            return Ok(parse_session_header_candidate(&line).flatten());
        }
    }
}

fn read_session_header_for_discovery(file_path: &str) -> Option<SessionHeader> {
    // Discovery is best-effort: unreadable or oversized files are not sessions,
    // and one corrupt file must not prevent other sessions from being found.
    read_session_header(file_path).ok().flatten()
}

fn session_cwd_matches(cwd: &str, resolved_cwd: &str) -> bool {
    !cwd.is_empty() && resolve(cwd) == resolved_cwd
}

fn mtime_ms(path: &str) -> Option<f64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(duration.as_secs_f64() * 1000.0)
}

fn list_jsonl_files(dir: &str) -> std::io::Result<Vec<String>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.ends_with(".jsonl") {
            files.push(name);
        }
    }
    Ok(files)
}

/// Exported for testing
pub fn find_most_recent_session(session_dir: &str, cwd: Option<&str>) -> Option<String> {
    let resolved_session_dir = normalize_path(session_dir, None);
    let resolved_cwd = cwd.filter(|c| !c.is_empty()).map(resolve);
    // Directory access and stat races make recent-session discovery unavailable.
    let names = list_jsonl_files(&resolved_session_dir).ok()?;
    let mut files = Vec::with_capacity(names.len());
    for name in names {
        let path = join_path(&resolved_session_dir, &name);
        let mtime = mtime_ms(&path)?;
        files.push((path, mtime));
    }
    files.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    for (path, _) in files {
        if let Some(header) = read_session_header_for_discovery(&path)
            && resolved_cwd
                .as_deref()
                .is_none_or(|resolved| session_cwd_matches(&header.cwd, resolved))
        {
            return Some(path);
        }
    }
    None
}

/// Join the text blocks of a JSON content value (`string | block[]`) with spaces.
fn extract_text_content_value(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .map(|block| match block.get("text") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Null) | None => String::new(),
                Some(other) => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn build_session_info_sync(
    file_path: &str,
    signal: Option<&CancellationToken>,
) -> anyhow::Result<Option<SessionInfo>> {
    let aborted = || signal.is_some_and(CancellationToken::is_cancelled);
    let inner = || -> std::io::Result<Option<SessionInfo>> {
        let metadata = std::fs::metadata(file_path)?;
        let file = std::fs::File::open(file_path)?;
        let reader = std::io::BufReader::new(file);
        let mut header: Option<Value> = None;
        let mut message_count = 0i64;
        let mut first_message = String::new();
        let mut all_messages: Vec<String> = Vec::new();
        let mut name: Option<String> = None;
        let mut last_activity_time: Option<i64> = None;

        for line in reader.split(b'\n') {
            if aborted() {
                return Err(std::io::Error::other("aborted"));
            }
            let line = line?;
            let line = String::from_utf8_lossy(&line);
            let line = line.strip_suffix('\r').unwrap_or(&line);
            let Some(entry) = parse_session_entry_line(line) else {
                continue;
            };

            if header.is_none() {
                if entry.get("type").and_then(Value::as_str) != Some("session") {
                    return Ok(None);
                }
                header = Some(entry);
                continue;
            }

            let entry_type = entry.get("type").and_then(Value::as_str);
            // Extract session name (use latest, including explicit clears)
            if entry_type == Some("session_info") {
                name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string);
            }

            if entry_type != Some("message") {
                continue;
            }
            message_count += 1;

            let Some(message) = entry.get("message").filter(|m| m.is_object()) else {
                continue;
            };
            let role = message.get("role").and_then(Value::as_str);
            let has_content = message.get("role").is_some_and(Value::is_string)
                && message.as_object().is_some_and(|m| m.contains_key("content"));
            if !has_content || !matches!(role, Some("user") | Some("assistant")) {
                continue;
            }

            let activity_time = match message.get("timestamp") {
                Some(Value::Number(n)) => n.as_f64().map(|f| f as i64),
                _ => entry
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(parse_iso_ms),
            };
            if let Some(activity_time) = activity_time {
                last_activity_time = Some(last_activity_time.unwrap_or(0).max(activity_time));
            }

            let text_content = extract_text_content_value(message.get("content").unwrap_or(&Value::Null));
            if text_content.is_empty() {
                continue;
            }
            if first_message.is_empty() && role == Some("user") {
                first_message = text_content.clone();
            }
            all_messages.push(text_content);
        }

        let Some(header) = header else {
            return Ok(None);
        };
        let Some(id) = header.get("id").and_then(Value::as_str) else {
            return Ok(None);
        };

        let cwd = header.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
        let parent_session_path = header
            .get("parentSession")
            .and_then(Value::as_str)
            .map(str::to_string);
        let header_time = header.get("timestamp").and_then(Value::as_str).and_then(parse_iso_ms);
        let mtime: chrono::DateTime<chrono::Utc> = metadata
            .modified()
            .map(chrono::DateTime::<chrono::Utc>::from)
            .unwrap_or_default();
        let modified = match (last_activity_time, header_time) {
            (Some(t), _) if t > 0 => ms_to_datetime(t),
            (_, Some(t)) => ms_to_datetime(t),
            _ => mtime,
        };
        let created = header_time.map(ms_to_datetime).unwrap_or(mtime);

        Ok(Some(SessionInfo {
            path: file_path.to_string(),
            id: id.to_string(),
            cwd,
            name,
            parent_session_path,
            created,
            modified,
            message_count,
            first_message: if first_message.is_empty() {
                "(no messages)".to_string()
            } else {
                first_message
            },
            all_messages_text: all_messages.join(" "),
        }))
    };
    match inner() {
        Ok(info) => Ok(info),
        Err(_) => {
            if aborted() {
                return Err(OperationAbortedError.into());
            }
            Ok(None)
        }
    }
}

async fn build_session_info(
    file_path: String,
    signal: Option<CancellationToken>,
) -> anyhow::Result<Option<SessionInfo>> {
    tokio::task::spawn_blocking(move || build_session_info_sync(&file_path, signal.as_ref()))
        .await
        .map_err(|e| anyhow::anyhow!(e))?
}

/// Progress callback: (loaded, total, sessions loaded so far sorted by activity on periodic updates).
pub type SessionListProgress = Arc<dyn Fn(i64, i64, Option<Vec<SessionInfo>>) + Send + Sync>;

const MAX_CONCURRENT_SESSION_INFO_LOADS: usize = 10;
const CURRENT_SESSION_LIST_PUBLISH_INTERVAL: i64 = 10;
const ALL_SESSION_LIST_PUBLISH_INTERVAL: i64 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionFileCandidate {
    pub path: String,
    /// Modification time in ms, when stat succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<f64>,
}

fn sort_session_infos(sessions: &mut [SessionInfo]) {
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
}

fn check_aborted(signal: Option<&CancellationToken>) -> anyhow::Result<()> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(OperationAbortedError.into());
    }
    Ok(())
}

/// Load session infos with bounded concurrency; `on_loaded(info, index)` runs as each finishes.
async fn build_session_infos_with_concurrency(
    files: &[SessionFileCandidate],
    mut on_loaded: impl FnMut(Option<&SessionInfo>, usize),
    signal: Option<&CancellationToken>,
) -> anyhow::Result<Vec<Option<SessionInfo>>> {
    let mut results: Vec<Option<SessionInfo>> = vec![None; files.len()];
    // Owned futures (no borrowed closure arguments) keep the outer future `Send`.
    let futures: Vec<_> = files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let signal = signal.cloned();
            let path = file.path.clone();
            async move { (index, build_session_info(path, signal).await) }
        })
        .collect();
    let mut stream = futures::stream::iter(futures).buffer_unordered(MAX_CONCURRENT_SESSION_INFO_LOADS);
    while let Some((index, result)) = stream.next().await {
        check_aborted(signal)?;
        let info = result?;
        on_loaded(info.as_ref(), index);
        results[index] = info;
    }
    Ok(results)
}

async fn list_sessions_from_dir(
    dir: &str,
    on_progress: Option<&SessionListProgress>,
    signal: Option<&CancellationToken>,
) -> anyhow::Result<Vec<SessionInfo>> {
    check_aborted(signal)?;
    if !exists(dir) {
        return Ok(Vec::new());
    }

    let mut names = match list_jsonl_files(dir) {
        Ok(names) => names,
        Err(_) => return Ok(Vec::new()),
    };
    names.sort_by(|a, b| pi_tui::collation::locale_compare(b, a));
    let files: Vec<SessionFileCandidate> = names
        .iter()
        .map(|name| SessionFileCandidate {
            path: join_path(dir, name),
            mtime_ms: None,
        })
        .collect();
    let total = files.len() as i64;
    let mut partial_sessions: Vec<SessionInfo> = Vec::new();
    let mut loaded = 0i64;
    let results = build_session_infos_with_concurrency(
        &files,
        |info, _| {
            loaded += 1;
            if let Some(info) = info {
                partial_sessions.push(info.clone());
            }
            let publish_partial =
                loaded == 1 || loaded % CURRENT_SESSION_LIST_PUBLISH_INTERVAL == 0 || loaded == total;
            if let Some(progress) = on_progress {
                let partial = publish_partial.then(|| {
                    let mut sorted = partial_sessions.clone();
                    sort_session_infos(&mut sorted);
                    sorted
                });
                progress(loaded, total, partial);
            }
        },
        signal,
    )
    .await;
    match results {
        Ok(results) => Ok(results.into_iter().flatten().collect()),
        Err(error) => {
            check_aborted(signal)?;
            Err(error)
        }
    }
}

// ============================================================================
// SessionManager
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
// Mirrors a TS union; boxing the large variants would add indirection to every
// construction and match across the crate for little memory saved.
#[allow(clippy::large_enum_variant)]
pub enum SessionManagerAppendMessageMessage {
    SystemMessage(SystemMessage),
    UserMessage(UserMessage),
    AssistantMessage(AssistantMessage),
    ToolResultMessage(ToolResultMessage),
    CustomMessage(CustomMessage),
    BashExecutionMessage(BashExecutionMessage),
}

impl From<SessionManagerAppendMessageMessage> for AgentMessage {
    fn from(value: SessionManagerAppendMessageMessage) -> Self {
        match value {
            SessionManagerAppendMessageMessage::SystemMessage(m) => m.into(),
            SessionManagerAppendMessageMessage::UserMessage(m) => m.into(),
            SessionManagerAppendMessageMessage::AssistantMessage(m) => m.into(),
            SessionManagerAppendMessageMessage::ToolResultMessage(m) => m.into(),
            SessionManagerAppendMessageMessage::CustomMessage(m) => m.into(),
            SessionManagerAppendMessageMessage::BashExecutionMessage(m) => m.into(),
        }
    }
}

pub type SessionManagerAppendContextEditReplacement = ContextEditEntryReplacement;

struct State {
    session_id: String,
    session_file: Option<String>,
    flushed: bool,
    file_entries: Vec<FileEntry>,
    /// Entry id → position in `file_entries` (latest entry with that id).
    by_id: IndexMap<String, usize>,
    labels_by_id: IndexMap<String, String>,
    label_timestamps_by_id: HashMap<String, String>,
    leaf_id: Option<String>,
}

impl State {
    fn get(&self, id: &str) -> Option<&SessionEntry> {
        self.by_id
            .get(id)
            .and_then(|&index| self.file_entries.get(index))
            .and_then(FileEntry::as_entry)
    }

    fn has(&self, id: &str) -> bool {
        self.by_id.contains_key(id)
    }

    fn entries(&self) -> impl Iterator<Item = &SessionEntry> {
        self.file_entries.iter().filter_map(FileEntry::as_entry)
    }

    fn header(&self) -> Option<&SessionHeader> {
        self.file_entries.iter().find_map(FileEntry::as_header)
    }

    fn generate_id(&self) -> String {
        generate_id(|id| self.has(id))
    }

    fn branch_path(&self, from_id: Option<&str>) -> Vec<&SessionEntry> {
        let start = from_id.or(self.leaf_id.as_deref());
        let mut path = Vec::new();
        let mut current = start.and_then(|id| self.get(id));
        let mut guard = 0usize;
        while let Some(entry) = current {
            path.push(entry);
            current = entry.parent_id().and_then(|parent| self.get(parent));
            guard += 1;
            if guard > 10_000_000 {
                break;
            }
        }
        path.reverse();
        path
    }

    fn session_path(&self) -> Vec<&SessionEntry> {
        let lookup = |id: &str| self.get(id);
        let last = self.entries().last();
        build_session_path_with(last, Some(self.leaf_id.as_deref()), &lookup)
    }

    fn has_conversation(&self) -> bool {
        self.entries().any(|entry| {
            matches!(entry, SessionEntry::Message(m) if matches!(m.message.role(), "user" | "assistant"))
        })
    }

    fn build_index(&mut self) {
        self.by_id.clear();
        self.labels_by_id.clear();
        self.label_timestamps_by_id.clear();
        self.leaf_id = None;
        for (index, file_entry) in self.file_entries.iter().enumerate() {
            let FileEntry::Entry(entry) = file_entry else {
                continue;
            };
            self.by_id.insert(entry.id().to_string(), index);
            self.leaf_id = Some(entry.id().to_string());
            if let SessionEntry::Label(label) = entry {
                match label.label.as_deref().filter(|l| !l.is_empty()) {
                    Some(text) => {
                        self.labels_by_id.insert(label.target_id.clone(), text.to_string());
                        self.label_timestamps_by_id
                            .insert(label.target_id.clone(), label.timestamp.clone());
                    }
                    None => {
                        self.labels_by_id.shift_remove(&label.target_id);
                        self.label_timestamps_by_id.remove(&label.target_id);
                    }
                }
            }
        }
    }
}

/// Manages conversation sessions as append-only trees stored in JSONL files.
///
/// Each session entry has an id and parentId forming a tree structure. The "leaf"
/// pointer tracks the current position. Appending creates a child of the current leaf.
/// Branching moves the leaf to an earlier entry, allowing new branches without
/// modifying history.
///
/// Use buildSessionContext() to get the resolved message list for the LLM, which
/// handles compaction summaries and follows the path from root to current leaf.
pub struct SessionManager {
    session_dir: String,
    cwd: String,
    persist: bool,
    state: Mutex<State>,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        f.debug_struct("SessionManager")
            .field("session_id", &state.session_id)
            .field("session_file", &state.session_file)
            .field("session_dir", &self.session_dir)
            .field("cwd", &self.cwd)
            .field("persist", &self.persist)
            .finish_non_exhaustive()
    }
}

impl SessionManager {
    pub fn new(
        cwd: &str,
        session_dir: &str,
        session_file: Option<&str>,
        persist: bool,
        new_session_options: Option<&NewSessionOptions>,
        preloaded_file_entries: Option<Vec<FileEntry>>,
    ) -> anyhow::Result<Self> {
        let session_dir = if session_dir.is_empty() {
            String::new()
        } else {
            normalize_path(session_dir, None)
        };
        let manager = Self {
            cwd: resolve(cwd),
            session_dir,
            persist,
            state: Mutex::new(State {
                session_id: String::new(),
                session_file: None,
                flushed: false,
                file_entries: Vec::new(),
                by_id: IndexMap::new(),
                labels_by_id: IndexMap::new(),
                label_timestamps_by_id: HashMap::new(),
                leaf_id: None,
            }),
        };
        if persist && !manager.session_dir.is_empty() && !exists(&manager.session_dir) {
            std::fs::create_dir_all(&manager.session_dir)?;
        }

        {
            let mut state = manager.state.lock();
            if let Some(session_file) = session_file {
                manager.set_session_file_locked(&mut state, session_file, preloaded_file_entries)?;
            } else if let Some(entries) = preloaded_file_entries.filter(|e| !e.is_empty()) {
                manager.load_entries_locked(&mut state, entries, new_session_options)?;
            } else {
                manager.new_session_locked(&mut state, new_session_options)?;
            }
        }
        Ok(manager)
    }

    /// Switch to a different session file (used for resume and branching)
    pub fn set_session_file(&self, session_file: &str) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        self.set_session_file_locked(&mut state, session_file, None)
    }

    fn set_session_file_locked(
        &self,
        state: &mut State,
        session_file: &str,
        preloaded_file_entries: Option<Vec<FileEntry>>,
    ) -> anyhow::Result<()> {
        let resolved = resolve(session_file);
        state.session_file = Some(resolved.clone());
        if exists(&resolved) {
            let entries = match preloaded_file_entries {
                Some(entries) => entries,
                None => load_entries_from_file(&resolved)?,
            };

            // If file was empty, initialize it with a valid session header. If it was
            // non-empty but did not parse as a pi session, fail without modifying it.
            if entries.is_empty() {
                if std::fs::metadata(&resolved)?.len() > 0 {
                    anyhow::bail!("Session file is not a valid {APP_NAME} session: {resolved}");
                }
                self.new_session_locked(state, None)?;
                state.session_file = Some(resolved);
                self.rewrite_file_locked(state)?;
                state.flushed = true;
                return Ok(());
            }

            self.load_entries_locked(state, entries, None)?;
            state.flushed = true;
        } else {
            self.new_session_locked(state, None)?;
            // preserve explicit path from --session flag
            state.session_file = Some(resolved);
        }
        Ok(())
    }

    pub fn new_session(&self, options: Option<&NewSessionOptions>) -> anyhow::Result<Option<String>> {
        let mut state = self.state.lock();
        self.new_session_locked(&mut state, options)
    }

    fn new_session_locked(
        &self,
        state: &mut State,
        options: Option<&NewSessionOptions>,
    ) -> anyhow::Result<Option<String>> {
        if let Some(id) = options.and_then(|o| o.id.as_deref()) {
            assert_valid_session_id(id)?;
        }
        state.session_id = options
            .and_then(|o| o.id.clone())
            .unwrap_or_else(create_session_id);
        let timestamp = now_iso();
        let header = SessionHeader {
            version: Some(CURRENT_SESSION_VERSION),
            id: state.session_id.clone(),
            timestamp: timestamp.clone(),
            cwd: self.cwd.clone(),
            parent_session: options.and_then(|o| o.parent_session.clone()),
        };
        state.file_entries = vec![FileEntry::Session(header)];
        state.by_id.clear();
        state.labels_by_id.clear();
        state.label_timestamps_by_id.clear();
        state.leaf_id = None;
        state.flushed = false;

        if self.persist {
            state.session_file = Some(join_path(
                &self.session_dir,
                &format!("{}_{}.jsonl", file_timestamp(&timestamp), state.session_id),
            ));
        }
        Ok(state.session_file.clone())
    }

    fn load_entries_locked(
        &self,
        state: &mut State,
        mut entries: Vec<FileEntry>,
        options: Option<&NewSessionOptions>,
    ) -> anyhow::Result<()> {
        let header_id = entries.iter().find_map(FileEntry::as_header).map(|h| h.id.clone());
        if let Some(header_id) = header_id {
            let migrated = migrate_to_current_version(&mut entries);
            state.file_entries = entries;
            state.session_id = header_id;
            if migrated {
                self.rewrite_file_locked(state)?;
            }
        } else {
            self.new_session_locked(state, options)?;
            state.file_entries.extend(entries);
        }

        state.build_index();
        Ok(())
    }

    fn rewrite_file_locked(&self, state: &State) -> anyhow::Result<()> {
        let Some(session_file) = state.session_file.as_deref().filter(|_| self.persist) else {
            return Ok(());
        };
        let mut content = String::new();
        for entry in &state.file_entries {
            content.push_str(&to_json_line(entry));
        }
        std::fs::write(session_file, content)?;
        Ok(())
    }

    pub fn is_persisted(&self) -> bool {
        self.persist
    }

    pub fn get_cwd(&self) -> String {
        self.cwd.clone()
    }

    pub fn get_session_dir(&self) -> String {
        self.session_dir.clone()
    }

    pub fn uses_default_session_dir(&self) -> bool {
        self.session_dir == get_default_session_dir_path(&self.cwd, None)
    }

    pub fn get_session_id(&self) -> String {
        self.state.lock().session_id.clone()
    }

    pub fn get_session_file(&self) -> Option<String> {
        self.state.lock().session_file.clone()
    }

    /// A new session file is created only once the session contains a user or assistant message.
    /// Setup entries alone (model, thinking level, system prompt) stay in memory so opening and
    /// closing pi without chatting leaves no file behind. Starting at the user message (not the
    /// first assistant reply) keeps the prompt on disk if the first turn never completes (#10000).
    fn persist_locked(&self, state: &mut State, entry: &SessionEntry) -> anyhow::Result<()> {
        if !self.persist {
            return Ok(());
        }
        let Some(session_file) = state.session_file.clone() else {
            return Ok(());
        };

        if !state.flushed {
            if !state.has_conversation() {
                return Ok(());
            }
            let mut content = String::new();
            for e in &state.file_entries {
                content.push_str(&to_json_line(e));
            }
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&session_file)?;
            file.write_all(content.as_bytes())?;
            state.flushed = true;
        } else {
            append_to_file(&session_file, &to_json_line(entry))?;
        }
        Ok(())
    }

    fn append_entry_locked(&self, state: &mut State, entry: SessionEntry) -> anyhow::Result<()> {
        let id = entry.id().to_string();
        state.file_entries.push(FileEntry::Entry(entry));
        let index = state.file_entries.len() - 1;
        state.by_id.insert(id.clone(), index);
        state.leaf_id = Some(id);
        let entry = state.file_entries[index]
            .as_entry()
            .cloned()
            .expect("just appended an entry");
        self.persist_locked(state, &entry)
    }

    /// Append a message as child of current leaf, then advance leaf. Returns entry id.
    /// Does not allow writing CompactionSummaryMessage and BranchSummaryMessage directly.
    /// Reason: we want these to be top-level entries in the session, not message session entries,
    /// so it is easier to find them.
    /// These need to be appended via appendCompaction() and appendBranchSummary() methods.
    pub fn append_message(&self, message: impl Into<AgentMessage>) -> anyhow::Result<String> {
        let mut state = self.state.lock();
        let id = state.generate_id();
        let entry = SessionEntry::Message(SessionMessageEntry {
            id: id.clone(),
            parent_id: state.leaf_id.clone(),
            timestamp: now_iso(),
            message: message.into(),
            extra: Map::new(),
        });
        self.append_entry_locked(&mut state, entry)?;
        Ok(id)
    }

    /// Append a thinking level change as child of current leaf, then advance leaf. Returns entry id.
    pub fn append_thinking_level_change(&self, thinking_level: &str) -> anyhow::Result<String> {
        let mut state = self.state.lock();
        let id = state.generate_id();
        let entry = SessionEntry::ThinkingLevelChange(ThinkingLevelChangeEntry {
            id: id.clone(),
            parent_id: state.leaf_id.clone(),
            timestamp: now_iso(),
            thinking_level: thinking_level.to_string(),
            extra: Map::new(),
        });
        self.append_entry_locked(&mut state, entry)?;
        Ok(id)
    }

    /// Append a model change as child of current leaf, then advance leaf. Returns entry id.
    pub fn append_model_change(&self, provider: &str, model_id: &str) -> anyhow::Result<String> {
        let mut state = self.state.lock();
        let id = state.generate_id();
        let entry = SessionEntry::ModelChange(ModelChangeEntry {
            id: id.clone(),
            parent_id: state.leaf_id.clone(),
            timestamp: now_iso(),
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            extra: Map::new(),
        });
        self.append_entry_locked(&mut state, entry)?;
        Ok(id)
    }

    /// Append model-attributed usage that does not participate in LLM context. Returns the appended entry.
    pub fn append_usage(
        &self,
        kind: &str,
        provider: &str,
        model: &str,
        usage: Usage,
        note: Option<&str>,
    ) -> anyhow::Result<UsageEntry> {
        let mut state = self.state.lock();
        let entry = UsageEntry {
            id: state.generate_id(),
            parent_id: state.leaf_id.clone(),
            timestamp: now_iso(),
            kind: kind.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            usage,
            note: note.filter(|n| !n.is_empty()).map(str::to_string),
            extra: Map::new(),
        };
        self.append_entry_locked(&mut state, SessionEntry::Usage(entry.clone()))?;
        Ok(entry)
    }

    /// Append a compaction summary as child of current leaf, then advance leaf. Returns entry id.
    pub fn append_compaction(
        &self,
        summary: &str,
        first_kept_entry_id: Option<&str>,
        tokens_before: u64,
        details: Option<Value>,
        from_hook: Option<bool>,
        usage: Option<Usage>,
    ) -> anyhow::Result<String> {
        let mut state = self.state.lock();
        let timestamp = now_iso();
        let projection = build_projection_from_path(state.session_path());
        let system_messages: Vec<Message> = projection
            .messages
            .iter()
            .filter_map(|m| m.as_system().cloned().map(Message::System))
            .collect();
        let system_message = pi_ai::get_current_system_message(&system_messages).map(|mut system| {
            system.timestamp = parse_timestamp_ms(&timestamp);
            system
        });
        let id = state.generate_id();
        let entry = SessionEntry::Compaction(CompactionEntry {
            id: id.clone(),
            parent_id: state.leaf_id.clone(),
            timestamp,
            summary: summary.to_string(),
            first_kept_entry_id: first_kept_entry_id.map(str::to_string).unwrap_or_else(|| id.clone()),
            tokens_before,
            details,
            usage,
            from_hook,
            system_message,
            extra: Map::new(),
        });
        self.append_entry_locked(&mut state, entry)?;
        Ok(id)
    }

    /// Append a custom entry (for extensions) as child of current leaf, then advance leaf. Returns entry id.
    pub fn append_custom_entry(&self, custom_type: &str, data: Option<Value>) -> anyhow::Result<String> {
        let mut state = self.state.lock();
        let id = state.generate_id();
        let entry = SessionEntry::Custom(CustomEntry {
            custom_type: custom_type.to_string(),
            data,
            id: id.clone(),
            parent_id: state.leaf_id.clone(),
            timestamp: now_iso(),
            extra: Map::new(),
        });
        self.append_entry_locked(&mut state, entry)?;
        Ok(id)
    }

    /// Append a session info entry (e.g., display name). Returns entry id.
    pub fn append_session_info(&self, name: &str) -> anyhow::Result<String> {
        static NEWLINES: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"[\r\n]+").expect("valid regex"));
        let sanitized_name = NEWLINES.replace_all(name, " ").trim().to_string();
        let mut state = self.state.lock();
        let id = state.generate_id();
        let entry = SessionEntry::SessionInfo(SessionInfoEntry {
            id: id.clone(),
            parent_id: state.leaf_id.clone(),
            timestamp: now_iso(),
            name: Some(sanitized_name),
            extra: Map::new(),
        });
        self.append_entry_locked(&mut state, entry)?;
        Ok(id)
    }

    /// Get the current session name from the latest session_info entry, if any.
    pub fn get_session_name(&self) -> Option<String> {
        // Walk entries in reverse to find the latest session_info entry.
        // Empty names explicitly clear the session title.
        let state = self.state.lock();
        for entry in state.file_entries.iter().rev() {
            if let FileEntry::Entry(SessionEntry::SessionInfo(info)) = entry {
                return info
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string);
            }
        }
        None
    }

    /// Append a custom message entry (for extensions) that participates in LLM context.
    /// @param customType Extension identifier for filtering on reload
    /// @param content Message content (string or TextContent/ImageContent array)
    /// @param display Whether to show in TUI (true = styled display, false = hidden)
    /// @param details Optional extension-specific metadata (not sent to LLM)
    /// @returns Entry id
    pub fn append_custom_message_entry(
        &self,
        custom_type: &str,
        content: UserContent,
        display: bool,
        details: Option<Value>,
    ) -> anyhow::Result<String> {
        let mut state = self.state.lock();
        let id = state.generate_id();
        let entry = SessionEntry::CustomMessage(CustomMessageEntry {
            custom_type: custom_type.to_string(),
            content,
            display,
            details,
            id: id.clone(),
            parent_id: state.leaf_id.clone(),
            timestamp: now_iso(),
            extra: Map::new(),
        });
        self.append_entry_locked(&mut state, entry)?;
        Ok(id)
    }

    /// Append a branch-local edit to an earlier model-visible entry.
    /// `None` omits the target from model context.
    pub fn append_context_edit(
        &self,
        target_id: &str,
        replacement: Option<&ContextEditEntryReplacement>,
    ) -> anyhow::Result<String> {
        let mut state = self.state.lock();
        let Some(target) = state.get(target_id) else {
            anyhow::bail!("Entry {target_id} not found");
        };
        if !state.branch_path(None).iter().any(|entry| entry.id() == target_id) {
            anyhow::bail!("Entry {target_id} is not on the active branch");
        }
        let target_role = match target {
            SessionEntry::CustomMessage(_) => "custom",
            SessionEntry::Message(m) if matches!(m.message.role(), "user" | "assistant" | "toolResult") => {
                m.message.role()
            }
            _ => anyhow::bail!("Entry {target_id} does not contribute editable model content"),
        };
        let normalized_replacement = replacement.map(|replacement| match &replacement.content {
            ContextEditableContent::User(UserContent::Text(text))
                if target_role == "assistant" || target_role == "toolResult" =>
            {
                ContextEditEntryReplacement {
                    content: ContextEditableContent::User(UserContent::Blocks(vec![
                        pi_ai::types::UserContentBlock::text(text.clone()),
                    ])),
                }
            }
            _ => replacement.clone(),
        });
        let id = state.generate_id();
        let entry = SessionEntry::ContextEdit(ContextEditEntry {
            id: id.clone(),
            parent_id: state.leaf_id.clone(),
            timestamp: now_iso(),
            target_id: target_id.to_string(),
            replacement: normalized_replacement,
            extra: Map::new(),
        });
        self.append_entry_locked(&mut state, entry)?;
        Ok(id)
    }

    // =========================================================================
    // Tree Traversal
    // =========================================================================

    pub fn get_leaf_id(&self) -> Option<String> {
        self.state.lock().leaf_id.clone()
    }

    pub fn get_leaf_entry(&self) -> Option<SessionEntry> {
        let state = self.state.lock();
        state.leaf_id.as_deref().and_then(|id| state.get(id)).cloned()
    }

    pub fn get_entry(&self, id: &str) -> Option<SessionEntry> {
        self.state.lock().get(id).cloned()
    }

    /// Get all direct children of an entry.
    pub fn get_children(&self, parent_id: &str) -> Vec<SessionEntry> {
        let state = self.state.lock();
        state
            .by_id
            .values()
            .filter_map(|&index| state.file_entries[index].as_entry())
            .filter(|entry| entry.parent_id() == Some(parent_id))
            .cloned()
            .collect()
    }

    /// Get the label for an entry, if any.
    pub fn get_label(&self, id: &str) -> Option<String> {
        self.state.lock().labels_by_id.get(id).cloned()
    }

    /// Set or clear a label on an entry.
    /// Labels are user-defined markers for bookmarking/navigation.
    /// Pass undefined or empty string to clear the label.
    pub fn append_label_change(&self, target_id: &str, label: Option<&str>) -> anyhow::Result<String> {
        let mut state = self.state.lock();
        if !state.has(target_id) {
            anyhow::bail!("Entry {target_id} not found");
        }
        let id = state.generate_id();
        let timestamp = now_iso();
        let entry = SessionEntry::Label(LabelEntry {
            id: id.clone(),
            parent_id: state.leaf_id.clone(),
            timestamp: timestamp.clone(),
            target_id: target_id.to_string(),
            label: label.map(str::to_string),
            extra: Map::new(),
        });
        self.append_entry_locked(&mut state, entry)?;
        match label.filter(|l| !l.is_empty()) {
            Some(label) => {
                state.labels_by_id.insert(target_id.to_string(), label.to_string());
                state.label_timestamps_by_id.insert(target_id.to_string(), timestamp);
            }
            None => {
                state.labels_by_id.shift_remove(target_id);
                state.label_timestamps_by_id.remove(target_id);
            }
        }
        Ok(id)
    }

    /// Walk from entry to root, returning all entries in path order.
    /// Includes all entry types (messages, compaction, model changes, etc.).
    /// Use buildSessionContext() to get the resolved messages for the LLM.
    pub fn get_branch(&self, from_id: Option<&str>) -> Vec<SessionEntry> {
        let state = self.state.lock();
        state.branch_path(from_id).into_iter().cloned().collect()
    }

    /// Build the active, compaction-aware entry list for context/rendering.
    /// Uses tree traversal from current leaf.
    pub fn build_context_entries(&self) -> Vec<SessionEntry> {
        let state = self.state.lock();
        build_context_entries_from_path(state.session_path())
            .into_iter()
            .cloned()
            .collect()
    }

    /// Build the session context (what gets sent to the LLM).
    /// Uses tree traversal from current leaf.
    pub fn build_session_projection(&self) -> SessionProjection {
        let state = self.state.lock();
        build_projection_from_path(state.session_path())
    }

    pub fn build_session_context(&self) -> SessionContext {
        let projection = self.build_session_projection();
        SessionContext {
            messages: projection.messages,
            thinking_level: projection.thinking_level,
            model: projection.model,
        }
    }

    /// Get session header.
    pub fn get_header(&self) -> Option<SessionHeader> {
        self.state.lock().header().cloned()
    }

    /// Number of session entries (excludes header), without copying them like `getEntries()`.
    pub fn get_entry_count(&self) -> i64 {
        self.state.lock().by_id.len() as i64
    }

    /// Get all session entries (excludes header). Returns a copy.
    /// The session is append-only: use appendXXX() to add entries, branch() to
    /// change the leaf pointer. Entries cannot be modified or deleted.
    pub fn get_entries(&self) -> Vec<SessionEntry> {
        self.state.lock().entries().cloned().collect()
    }

    /// Raw file entries (header, tree entries and preserved unknown entries) in file order.
    pub fn get_file_entries(&self) -> Vec<FileEntry> {
        self.state.lock().file_entries.clone()
    }

    /// Get the session as a tree structure. Returns a defensive copy of all entries.
    /// A well-formed session has exactly one root (first entry with parentId === null).
    /// Orphaned entries (broken parent chain) are also returned as roots.
    pub fn get_tree(&self) -> Vec<SessionTreeNode> {
        let state = self.state.lock();
        let entries: Vec<&SessionEntry> = state.entries().collect();
        // Node per entry id (later duplicates replace earlier ones, like Map.set).
        let mut node_index: HashMap<&str, usize> = HashMap::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            node_index.insert(entry.id(), index);
        }

        let mut children: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
        let mut roots: Vec<usize> = Vec::new();
        for entry in &entries {
            let node = node_index[entry.id()];
            match entry.parent_id() {
                None => roots.push(node),
                Some(parent_id) if parent_id == entry.id() => roots.push(node),
                Some(parent_id) => match node_index.get(parent_id) {
                    Some(&parent) => children[parent].push(node),
                    // Orphan - treat as root
                    None => roots.push(node),
                },
            }
        }

        // Sort children by timestamp (oldest first, newest at bottom)
        let times: Vec<i64> = entries.iter().map(|e| parse_timestamp_ms(e.timestamp())).collect();
        for list in children.iter_mut() {
            list.sort_by_key(|&index| times[index]);
        }

        // Pre-order traversal from roots (iterative), then build bottom-up.
        let mut order: Vec<usize> = Vec::new();
        let mut visited = vec![false; entries.len()];
        let mut stack: Vec<usize> = roots.iter().rev().copied().collect();
        while let Some(node) = stack.pop() {
            if visited[node] {
                continue;
            }
            visited[node] = true;
            order.push(node);
            stack.extend(children[node].iter().rev().copied());
        }
        let mut built: Vec<Option<SessionTreeNode>> = (0..entries.len()).map(|_| None).collect();
        for &node in order.iter().rev() {
            let entry = entries[node];
            let node_children = children[node]
                .iter()
                .filter_map(|&child| built[child].take())
                .collect();
            built[node] = Some(SessionTreeNode {
                entry: entry.clone(),
                children: node_children,
                label: state.labels_by_id.get(entry.id()).cloned(),
                label_timestamp: state.label_timestamps_by_id.get(entry.id()).cloned(),
            });
        }
        roots.iter().filter_map(|&root| built[root].take()).collect()
    }

    // =========================================================================
    // Branching
    // =========================================================================

    /// Start a new branch from an earlier entry.
    /// Moves the leaf pointer to the specified entry. The next appendXXX() call
    /// will create a child of that entry, forming a new branch. Existing entries
    /// are not modified or deleted.
    pub fn branch(&self, branch_from_id: &str) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        if !state.has(branch_from_id) {
            anyhow::bail!("Entry {branch_from_id} not found");
        }
        state.leaf_id = Some(branch_from_id.to_string());
        Ok(())
    }

    /// Reset the leaf pointer to null (before any entries).
    /// The next appendXXX() call will create a new root entry (parentId = null).
    /// Use this when navigating to re-edit the first user message.
    pub fn reset_leaf(&self) {
        self.state.lock().leaf_id = None;
    }

    /// Start a new branch with a summary of the abandoned path.
    /// Same as branch(), but also appends a branch_summary entry that captures
    /// context from the abandoned conversation path.
    pub fn branch_with_summary(
        &self,
        branch_from_id: Option<&str>,
        summary: &str,
        details: Option<Value>,
        from_hook: Option<bool>,
        usage: Option<Usage>,
    ) -> anyhow::Result<String> {
        let mut state = self.state.lock();
        if let Some(branch_from_id) = branch_from_id
            && !state.has(branch_from_id)
        {
            anyhow::bail!("Entry {branch_from_id} not found");
        }
        let from_id = state.leaf_id.clone().unwrap_or_else(|| "root".to_string());
        state.leaf_id = branch_from_id.map(str::to_string);
        let id = state.generate_id();
        let entry = SessionEntry::BranchSummary(BranchSummaryEntry {
            id: id.clone(),
            parent_id: branch_from_id.map(str::to_string),
            timestamp: now_iso(),
            from_id,
            summary: summary.to_string(),
            details,
            usage,
            from_hook,
            extra: Map::new(),
        });
        self.append_entry_locked(&mut state, entry)?;
        Ok(id)
    }

    /// Create a new session file containing only the path from root to the specified leaf.
    /// Useful for extracting a single conversation path from a branched session.
    /// Returns the new session file path, or undefined if not persisting.
    pub fn create_branched_session(&self, leaf_id: &str) -> anyhow::Result<Option<String>> {
        let mut state = self.state.lock();
        let previous_session_file = state.session_file.clone();
        let path: Vec<SessionEntry> = state.branch_path(Some(leaf_id)).into_iter().cloned().collect();
        if path.is_empty() {
            anyhow::bail!("Entry {leaf_id} not found");
        }

        // Filter out LabelEntry from path - we'll recreate them from the resolved map.
        // Because labels are real tree entries, later entries can be children of labels;
        // removing labels requires re-chaining the retained path to avoid orphaned subtrees.
        let mut path_without_labels: Vec<SessionEntry> = Vec::new();
        let mut replacement_by_label_id: HashMap<String, String> = HashMap::new();
        let mut pending_label_ids: Vec<String> = Vec::new();
        let mut path_parent_id: Option<String> = None;
        for entry in path {
            if let SessionEntry::Label(_) = entry {
                pending_label_ids.push(entry.id().to_string());
                continue;
            }
            for label_id in pending_label_ids.drain(..) {
                replacement_by_label_id.insert(label_id, entry.id().to_string());
            }
            let entry_id = entry.id().to_string();
            let mut copy = entry;
            copy.set_parent_id(path_parent_id.clone());
            if let SessionEntry::Compaction(compaction) = &mut copy
                && compaction.first_kept_entry_id != compaction.id
                && let Some(replacement) = replacement_by_label_id.get(&compaction.first_kept_entry_id)
            {
                compaction.first_kept_entry_id = replacement.clone();
            }
            path_without_labels.push(copy);
            path_parent_id = Some(entry_id);
        }

        let new_session_id = create_session_id();
        let timestamp = now_iso();
        let new_session_file = join_path(
            &self.session_dir,
            &format!("{}_{}.jsonl", file_timestamp(&timestamp), new_session_id),
        );

        let header = SessionHeader {
            version: Some(CURRENT_SESSION_VERSION),
            id: new_session_id.clone(),
            timestamp,
            cwd: self.cwd.clone(),
            parent_session: if self.persist { previous_session_file } else { None },
        };

        // Collect labels for entries in the path
        let mut path_entry_ids: HashSet<String> =
            path_without_labels.iter().map(|e| e.id().to_string()).collect();
        let labels_to_write: Vec<(String, String, String)> = state
            .labels_by_id
            .iter()
            .filter(|(target_id, _)| path_entry_ids.contains(target_id.as_str()))
            .map(|(target_id, label)| {
                (
                    target_id.clone(),
                    label.clone(),
                    state
                        .label_timestamps_by_id
                        .get(target_id)
                        .cloned()
                        .unwrap_or_default(),
                )
            })
            .collect();

        // Build label entries
        let mut parent_id = path_without_labels
            .last()
            .map(|e| e.id().to_string())
            .filter(|id| !id.is_empty());
        let mut label_entries: Vec<SessionEntry> = Vec::new();
        for (target_id, label, label_timestamp) in labels_to_write {
            let id = generate_id(|candidate| path_entry_ids.contains(candidate));
            path_entry_ids.insert(id.clone());
            label_entries.push(SessionEntry::Label(LabelEntry {
                id: id.clone(),
                parent_id: parent_id.clone(),
                timestamp: label_timestamp,
                target_id,
                label: Some(label),
                extra: Map::new(),
            }));
            parent_id = Some(id);
        }

        let mut file_entries = vec![FileEntry::Session(header)];
        file_entries.extend(path_without_labels.into_iter().map(FileEntry::Entry));
        file_entries.extend(label_entries.into_iter().map(FileEntry::Entry));
        state.file_entries = file_entries;
        state.session_id = new_session_id;
        state.build_index();

        if self.persist {
            state.session_file = Some(new_session_file.clone());
            // Use the same rule as _persist(): write now if the branched path already
            // has a conversation, otherwise let _persist() create the file later.
            if state.has_conversation() {
                self.rewrite_file_locked(&state)?;
                state.flushed = true;
            } else {
                state.flushed = false;
            }
            return Ok(Some(new_session_file));
        }

        // In-memory mode: replace current session with the path + labels
        Ok(None)
    }

    /// Create a new session.
    /// @param cwd Working directory (stored in session header)
    /// @param sessionDir Optional session directory. If omitted, uses default (~/.pi/agent/sessions/<encoded-cwd>/).
    pub fn create(
        cwd: &str,
        session_dir: Option<&str>,
        options: Option<&NewSessionOptions>,
    ) -> anyhow::Result<Arc<SessionManager>> {
        let dir = match session_dir.filter(|d| !d.is_empty()) {
            Some(dir) => normalize_path(dir, None),
            None => get_default_session_dir(cwd, None)?,
        };
        Ok(Arc::new(Self::new(cwd, &dir, None, true, options, None)?))
    }

    /// Open a specific session file.
    /// @param path Path to session file
    /// @param sessionDir Optional session directory for /new or /branch. If omitted, derives from file's parent.
    /// @param cwdOverride Optional cwd override instead of the session header cwd.
    pub fn open(
        path: &str,
        session_dir: Option<&str>,
        cwd_override: Option<&str>,
    ) -> anyhow::Result<Arc<SessionManager>> {
        let resolved_path = resolve(path);
        let mut header: Option<SessionHeader> = None;
        let mut preloaded_file_entries: Option<Vec<FileEntry>> = None;
        if cwd_override.is_none() && exists(&resolved_path) {
            match read_session_header(&resolved_path) {
                Ok(h) => header = h,
                Err(error) => {
                    if error.downcast_ref::<SessionHeaderScanLimitError>().is_none() {
                        return Err(error);
                    }
                    // The bounded scan is only a discovery optimization. A full load remains
                    // authoritative for legacy files with very large headers or prefixes.
                    let entries = load_entries_from_file(&resolved_path)?;
                    header = entries.first().and_then(FileEntry::as_header).cloned();
                    preloaded_file_entries = Some(entries);
                }
            }
        }
        let cwd = cwd_override
            .map(str::to_string)
            .or_else(|| header.map(|h| h.cwd))
            .unwrap_or_else(current_dir);
        // If no sessionDir provided, derive from file's parent directory
        let dir = match session_dir.filter(|d| !d.is_empty()) {
            Some(dir) => normalize_path(dir, None),
            None => Path::new(&resolved_path)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".to_string()),
        };
        Ok(Arc::new(Self::new(
            &cwd,
            &dir,
            Some(&resolved_path),
            true,
            None,
            preloaded_file_entries,
        )?))
    }

    /// Continue the most recent session, or create new if none.
    /// @param cwd Working directory
    /// @param sessionDir Optional session directory. If omitted, uses default (~/.pi/agent/sessions/<encoded-cwd>/).
    pub fn continue_recent(cwd: &str, session_dir: Option<&str>) -> anyhow::Result<Arc<SessionManager>> {
        let dir = match session_dir.filter(|d| !d.is_empty()) {
            Some(dir) => normalize_path(dir, None),
            None => get_default_session_dir(cwd, None)?,
        };
        let filter_cwd = session_dir.is_some() && dir != get_default_session_dir_path(cwd, None);
        let most_recent = find_most_recent_session(&dir, if filter_cwd { Some(cwd) } else { None });
        Ok(Arc::new(Self::new(
            cwd,
            &dir,
            most_recent.as_deref(),
            true,
            None,
            None,
        )?))
    }

    /// Create an in-memory session (no file persistence), optionally from entries held outside the filesystem.
    pub fn in_memory(
        cwd: Option<&str>,
        options: Option<&NewSessionOptions>,
        entries: Option<Vec<FileEntry>>,
    ) -> anyhow::Result<Arc<SessionManager>> {
        let cwd = cwd.map(str::to_string).unwrap_or_else(current_dir);
        Ok(Arc::new(Self::new(&cwd, "", None, false, options, entries)?))
    }

    /// Fork a session from another project directory into the current project.
    /// Creates a new session in the target cwd with the full history from the source session.
    /// @param sourcePath Path to the source session file
    /// @param targetCwd Target working directory (where the new session will be stored)
    /// @param sessionDir Optional session directory. If omitted, uses default for targetCwd.
    pub fn fork_from(
        source_path: &str,
        target_cwd: &str,
        session_dir: Option<&str>,
        options: Option<&NewSessionOptions>,
    ) -> anyhow::Result<Arc<SessionManager>> {
        let resolved_source_path = resolve(source_path);
        let resolved_target_cwd = resolve(target_cwd);
        let source_entries = load_entries_from_file(&resolved_source_path)?;
        if source_entries.is_empty() {
            anyhow::bail!("Cannot fork: source session file is empty or invalid: {resolved_source_path}");
        }

        if !source_entries.iter().any(|e| matches!(e, FileEntry::Session(_))) {
            anyhow::bail!("Cannot fork: source session has no header: {resolved_source_path}");
        }

        let dir = match session_dir.filter(|d| !d.is_empty()) {
            Some(dir) => normalize_path(dir, None),
            None => get_default_session_dir(&resolved_target_cwd, None)?,
        };
        if !exists(&dir) {
            std::fs::create_dir_all(&dir)?;
        }

        // Create new session file with new ID but forked content
        if let Some(id) = options.and_then(|o| o.id.as_deref()) {
            assert_valid_session_id(id)?;
        }
        let new_session_id = options
            .and_then(|o| o.id.clone())
            .unwrap_or_else(create_session_id);
        let timestamp = now_iso();
        let new_session_file = join_path(
            &dir,
            &format!("{}_{}.jsonl", file_timestamp(&timestamp), new_session_id),
        );

        // Write new header pointing to source as parent, with updated cwd
        let new_header = FileEntry::Session(SessionHeader {
            version: Some(CURRENT_SESSION_VERSION),
            id: new_session_id,
            timestamp,
            cwd: resolved_target_cwd.clone(),
            parent_session: Some(resolved_source_path),
        });
        let mut content = to_json_line(&new_header);
        // Copy all non-header entries from source
        for entry in &source_entries {
            if !matches!(entry, FileEntry::Session(_)) {
                content.push_str(&to_json_line(entry));
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&new_session_file)?;
        file.write_all(content.as_bytes())?;
        drop(file);

        Ok(Arc::new(Self::new(
            &resolved_target_cwd,
            &dir,
            Some(&new_session_file),
            true,
            None,
            None,
        )?))
    }

    /// Find an exact session ID without loading transcript bodies.
    /// @param cwd Working directory (used to compute default session directory)
    /// @param id Exact session ID
    /// @param sessionDir Optional session directory. If omitted, uses default (~/.pi/agent/sessions/<encoded-cwd>/).
    pub fn find_by_id(cwd: &str, id: &str, session_dir: Option<&str>) -> anyhow::Result<Option<String>> {
        let dir = match session_dir.filter(|d| !d.is_empty()) {
            Some(dir) => normalize_path(dir, None),
            None => get_default_session_dir(cwd, None)?,
        };
        let filter_cwd = session_dir.is_some() && dir != get_default_session_dir_path(cwd, None);
        let resolved_cwd = resolve(cwd);

        // Exact session discovery is best-effort, matching list().
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(None);
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return Ok(None);
            };
            let file = entry.file_name().to_string_lossy().into_owned();
            if !file.ends_with(".jsonl") {
                continue;
            }
            let path = join_path(&dir, &file);
            let Some(header) = read_session_header_for_discovery(&path) else {
                continue;
            };
            if header.id != id {
                continue;
            }
            if filter_cwd && !session_cwd_matches(&header.cwd, &resolved_cwd) {
                continue;
            }
            return Ok(Some(path));
        }
        Ok(None)
    }

    /// List all sessions for a directory.
    /// @param cwd Working directory (used to compute default session directory)
    /// @param sessionDir Optional session directory. If omitted, uses default (~/.pi/agent/sessions/<encoded-cwd>/).
    /// @param onProgress Optional callback for progress updates (loaded, total)
    pub async fn list(
        cwd: &str,
        session_dir: Option<&str>,
        on_progress: Option<SessionListProgress>,
        signal: Option<CancellationToken>,
    ) -> anyhow::Result<Vec<SessionInfo>> {
        let dir = match session_dir.filter(|d| !d.is_empty()) {
            Some(dir) => normalize_path(dir, None),
            None => get_default_session_dir(cwd, None)?,
        };
        let filter_cwd = session_dir.is_some() && dir != get_default_session_dir_path(cwd, None);
        let resolved_cwd = resolve(cwd);
        let include_session = {
            let resolved_cwd = resolved_cwd.clone();
            move |session: &SessionInfo| !filter_cwd || session_cwd_matches(&session.cwd, &resolved_cwd)
        };
        let progress: Option<SessionListProgress> = on_progress.map(|on_progress| {
            let include_session = include_session.clone();
            Arc::new(move |loaded: i64, total: i64, partial: Option<Vec<SessionInfo>>| {
                on_progress(
                    loaded,
                    total,
                    partial.map(|sessions| sessions.into_iter().filter(|s| include_session(s)).collect()),
                )
            }) as SessionListProgress
        });
        let mut sessions: Vec<SessionInfo> = list_sessions_from_dir(&dir, progress.as_ref(), signal.as_ref())
            .await?
            .into_iter()
            .filter(|s| include_session(s))
            .collect();
        sort_session_infos(&mut sessions);
        Ok(sessions)
    }

    /// List all sessions across all project directories (or in `session_dir` when given).
    /// @param onProgress Optional callback for progress updates (loaded, total)
    pub async fn list_all(
        session_dir: Option<&str>,
        on_progress: Option<SessionListProgress>,
        signal: Option<CancellationToken>,
    ) -> anyhow::Result<Vec<SessionInfo>> {
        let signal = signal.as_ref();
        check_aborted(signal)?;
        if let Some(custom_session_dir) = session_dir {
            let custom_session_dir = normalize_path(custom_session_dir, None);
            let mut sessions = list_sessions_from_dir(&custom_session_dir, on_progress.as_ref(), signal).await?;
            sort_session_infos(&mut sessions);
            return Ok(sessions);
        }

        let sessions_dir = get_sessions_dir();
        let result: anyhow::Result<Vec<SessionInfo>> = async {
            if !exists(&sessions_dir) {
                return Ok(Vec::new());
            }
            let mut dirs = Vec::new();
            for entry in std::fs::read_dir(&sessions_dir)? {
                let entry = entry?;
                let file_type = entry.file_type()?;
                if file_type.is_dir() || file_type.is_symlink() {
                    dirs.push(join_path(&sessions_dir, &entry.file_name().to_string_lossy()));
                }
            }

            let mut all_files = Vec::new();
            for dir in &dirs {
                check_aborted(signal)?;
                if let Ok(names) = list_jsonl_files(dir) {
                    all_files.extend(names.iter().map(|name| join_path(dir, name)));
                }
            }
            let mut candidates: Vec<SessionFileCandidate> = all_files
                .into_iter()
                .map(|path| {
                    let mtime_ms = mtime_ms(&path);
                    SessionFileCandidate { path, mtime_ms }
                })
                .collect();
            check_aborted(signal)?;
            candidates.sort_by(|a, b| {
                let a_time = a.mtime_ms.unwrap_or(f64::NEG_INFINITY);
                let b_time = b.mtime_ms.unwrap_or(f64::NEG_INFINITY);
                b_time
                    .partial_cmp(&a_time)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        let name = |p: &str| Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        pi_tui::collation::locale_compare(&name(&b.path), &name(&a.path))
                    })
            });

            let total_files = candidates.len() as i64;
            let mut loaded = 0i64;
            let mut first_candidate_loaded = false;
            let mut partial_sessions: Vec<SessionInfo> = Vec::new();
            let results = build_session_infos_with_concurrency(
                &candidates,
                |info, index| {
                    loaded += 1;
                    if index == 0 {
                        first_candidate_loaded = true;
                    }
                    if let Some(info) = info {
                        partial_sessions.push(info.clone());
                    }
                    let publish_partial = first_candidate_loaded
                        && (index == 0 || loaded % ALL_SESSION_LIST_PUBLISH_INTERVAL == 0 || loaded == total_files);
                    if let Some(progress) = &on_progress {
                        let partial = publish_partial.then(|| {
                            let mut sorted = partial_sessions.clone();
                            sort_session_infos(&mut sorted);
                            sorted
                        });
                        progress(loaded, total_files, partial);
                    }
                },
                signal,
            )
            .await?;

            let mut sessions: Vec<SessionInfo> = results.into_iter().flatten().collect();
            sort_session_infos(&mut sessions);
            Ok(sessions)
        }
        .await;
        match result {
            Ok(sessions) => Ok(sessions),
            Err(_) => {
                check_aborted(signal)?;
                Ok(Vec::new())
            }
        }
    }
}

impl crate::core::session_cwd::SessionCwdSource for SessionManager {
    fn get_cwd(&self) -> String {
        SessionManager::get_cwd(self)
    }

    fn get_session_file(&self) -> Option<String> {
        SessionManager::get_session_file(self)
    }
}
