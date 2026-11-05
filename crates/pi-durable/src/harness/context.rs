//! Active transcript and model context, bounded by immutable committed entry IDs.
//! Incremental reads retain immutable messages and only rescan the appended tail.
use crate::{
    storage::{StorageFuture, line::eager_on},
    wire, *,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

type Message = Arc<JsonValue>;
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ContextView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<Arc<EntryRecord>>,
    pub entries: Vec<Arc<EntryRecord>>,
    pub contributions: Vec<Vec<Message>>,
    pub messages: Vec<Message>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextBounds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<Arc<EntryRecord>>,
    pub tail: EntryId,
}
#[derive(Clone)]
struct ContextEntry {
    record: Arc<EntryRecord>,
    model: Vec<Message>,
    edits: Vec<(EntryId, Option<Vec<Message>>)>,
}
impl ContextEntry {
    fn new(record: EntryRecord) -> Self {
        let model = record
            .model
            .as_ref()
            .map(|model| model.iter().cloned().map(Arc::new).collect())
            .unwrap_or_default();
        let edits = record
            .edits
            .as_ref()
            .into_iter()
            .flatten()
            .map(|edit| match edit {
                ContextEdit::Omit { target } => (*target, None),
                ContextEdit::Replace { target, messages } => (
                    *target,
                    Some(messages.iter().cloned().map(Arc::new).collect()),
                ),
            })
            .collect();
        Self {
            record: Arc::new(record),
            model,
            edits,
        }
    }
}
/// None means a later entry can still be the baseline; Some(None) rules it out.
type Baseline = Option<Option<Message>>;
#[derive(Clone)]
pub struct ContextRange {
    pub bounds: ContextBounds,
    entries: Vec<ContextEntry>,
    pub view: ContextView,
    edited: Arc<HashSet<EntryId>>,
    settled: Vec<Message>,
    open: Vec<Message>,
    baseline: Baseline,
}
pub struct ContextRead {
    pub view: ContextView,
    pub range: Option<Arc<ContextRange>>,
}

/// Capture on the Session line. The fixed tail makes the subsequent scans safe
/// outside the line, even if later commits append, edit, reset, or fork history.
pub async fn capture_context_bounds(
    storage: &dyn Storage,
    conversation: ConversationId,
    context: &Context,
    at: Option<EntryId>,
) -> Result<Option<ContextBounds>> {
    let tail = if let Some(at) = at {
        if storage.entry_in(conversation, at, context).await?.is_none() {
            return Err(Error::message(format!(
                "Entry {at} is not visible from conversation {conversation}"
            )));
        }
        at
    } else {
        let page = storage
            .scan_entries(
                &EntryQuery {
                    conversation_id: conversation,
                    min_entry_id: None,
                    max_entry_id: None,
                    order: None,
                },
                1,
                None,
                context,
            )
            .await?;
        let Some(entry) = page.items.first() else {
            return Ok(None);
        };
        entry.id
    };
    Ok(Some(ContextBounds {
        head: storage
            .find_latest_head_marker(conversation, Some(tail), context)
            .await?
            .map(Arc::new),
        tail,
    }))
}

pub fn read_context(
    session: &Session,
    storage: Arc<dyn Storage>,
    conversation: ConversationId,
    context: &Context,
    at: Option<EntryId>,
) -> StorageFuture<ContextView> {
    let read = read_context_from(session, storage, conversation, context, at, None);
    Box::pin(async move { Ok(read.await?.view) })
}
pub fn read_context_from(
    session: &Session,
    storage: Arc<dyn Storage>,
    conversation: ConversationId,
    context: &Context,
    at: Option<EntryId>,
    previous: Option<Arc<ContextRange>>,
) -> StorageFuture<ContextRead> {
    let on_line = storage.clone();
    let bounds_context = context.clone();
    let bounds = session.read_on_line(move || async move {
        capture_context_bounds(on_line.as_ref(), conversation, &bounds_context, at).await
    });
    let context = context.clone();
    // Continue on the same FIFO as bounds capture, before later close work can
    // reach the backend. The range scan still releases the mutation line.
    eager_on(session, async move {
        read_range(
            storage.as_ref(),
            conversation,
            bounds.await?,
            &context,
            previous,
        )
        .await
    })
}

/// Derive a captured range, optionally extending a retained range of the same
/// conversation. Hosts capturing bounds themselves use this on the other side
/// of their serialized admission boundary.
pub async fn read_range(
    storage: &dyn Storage,
    conversation: ConversationId,
    bounds: Option<ContextBounds>,
    context: &Context,
    previous: Option<Arc<ContextRange>>,
) -> Result<ContextRead> {
    let Some(bounds) = bounds else {
        return Ok(ContextRead {
            view: ContextView::default(),
            range: None,
        });
    };
    let previous = previous.filter(|previous| {
        previous.bounds.head.as_ref().map(|head| head.id)
            == bounds.head.as_ref().map(|head| head.id)
    });
    let range = if let Some(previous) = previous {
        if bounds.tail == previous.bounds.tail {
            previous
        } else if bounds.tail < previous.bounds.tail {
            Arc::new(derive_range(
                bounds.clone(),
                previous
                    .entries
                    .iter()
                    .filter(|entry| entry.record.id <= bounds.tail)
                    .cloned()
                    .collect(),
            ))
        } else {
            let added = scan_range(
                storage,
                conversation,
                Some(EntryId::from_raw(previous.bounds.tail.get() + 1)),
                bounds.tail,
                context,
            )
            .await?;
            Arc::new(extend_range(&previous, bounds, added))
        }
    } else {
        let entries = scan_range(
            storage,
            conversation,
            range_start(&bounds)?,
            bounds.tail,
            context,
        )
        .await?;
        Arc::new(derive_range(bounds, entries))
    };
    Ok(ContextRead {
        view: range.view.clone(),
        range: Some(range),
    })
}
pub async fn derive_context(
    storage: &dyn Storage,
    conversation: ConversationId,
    bounds: Option<ContextBounds>,
    context: &Context,
) -> Result<ContextView> {
    Ok(read_range(storage, conversation, bounds, context, None)
        .await?
        .view)
}
pub async fn active_entries(
    storage: &dyn Storage,
    conversation: ConversationId,
    bounds: Option<ContextBounds>,
    context: &Context,
) -> Result<Vec<Arc<EntryRecord>>> {
    let Some(bounds) = bounds else {
        return Ok(Vec::new());
    };
    let entries = scan_range(
        storage,
        conversation,
        range_start(&bounds)?,
        bounds.tail,
        context,
    )
    .await?;
    Ok(select_active(&bounds, &entries)
        .into_iter()
        .map(|entry| entry.record)
        .collect())
}
fn range_start(bounds: &ContextBounds) -> Result<Option<EntryId>> {
    bounds
        .head
        .as_ref()
        .map(|head| {
            head.head
                .ok_or_else(|| Error::message("Context head marker has no head"))
        })
        .transpose()
}
async fn scan_range(
    storage: &dyn Storage,
    conversation: ConversationId,
    min_entry_id: Option<EntryId>,
    tail: EntryId,
    context: &Context,
) -> Result<Vec<ContextEntry>> {
    let query = EntryQuery {
        conversation_id: conversation,
        min_entry_id,
        max_entry_id: Some(tail),
        order: None,
    };
    let mut entries = Vec::new();
    let mut cursor = None;
    loop {
        let page = storage
            .scan_entries(&query, 256, cursor.as_ref(), context)
            .await?;
        entries.extend(page.items.into_iter().map(ContextEntry::new));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    entries.reverse();
    Ok(entries)
}
fn select_active(bounds: &ContextBounds, range: &[ContextEntry]) -> Vec<ContextEntry> {
    if let Some(head) = &bounds.head {
        std::iter::once(ContextEntry::new((**head).clone()))
            .chain(
                range
                    .iter()
                    .filter(|entry| entry.record.head.is_none())
                    .cloned(),
            )
            .collect()
    } else {
        range.to_vec()
    }
}
fn role(message: &JsonValue, wanted: &str) -> bool {
    wire::get(message, "role").is_some_and(|role| role.string_equals(wanted))
}
fn contribute(entry: &ContextEntry, edit: Option<&Option<Vec<Message>>>) -> Vec<Message> {
    let messages = match edit {
        Some(None) => return Vec::new(),
        Some(Some(messages)) => messages,
        None => &entry.model,
    };
    messages
        .iter()
        .filter(|message| {
            !role(message, "assistant")
                || !wire::get(message, "stopReason").is_some_and(|reason| {
                    ["aborted", "error", "deferred"]
                        .iter()
                        .any(|excluded| reason.string_equals(excluded))
                })
        })
        .cloned()
        .collect()
}
fn derive_range(bounds: ContextBounds, entries: Vec<ContextEntry>) -> ContextRange {
    let mut edits = HashMap::new();
    for entry in &entries {
        for (target, messages) in &entry.edits {
            edits.insert(*target, messages);
        }
    }
    let active = select_active(&bounds, &entries);
    let contributions = active
        .iter()
        .map(|entry| contribute(entry, edits.get(&entry.record.id).copied()))
        .collect::<Vec<_>>();
    let (settled, open) = settle(
        Vec::new(),
        contributions.iter().flatten().cloned().collect(),
    );
    let baseline = find_baseline(&bounds, &active);
    let messages = lead_with_system(
        settled
            .iter()
            .cloned()
            .chain(order_tool_results(&open))
            .collect(),
        &baseline,
    );
    let edited = Arc::new(edits.keys().copied().collect());
    let view = ContextView {
        head: bounds.head.clone(),
        entries: active.into_iter().map(|entry| entry.record).collect(),
        contributions,
        messages,
    };
    ContextRange {
        bounds,
        entries,
        view,
        edited,
        settled,
        open,
        baseline,
    }
}
fn extend_range(
    previous: &ContextRange,
    bounds: ContextBounds,
    added: Vec<ContextEntry>,
) -> ContextRange {
    let mut entries = previous.entries.clone();
    entries.extend(added.iter().cloned());
    if added.iter().any(|entry| {
        entry.record.edits.is_some()
            || entry.record.head.is_some()
            || previous.edited.contains(&entry.record.id)
    }) {
        return derive_range(bounds, entries);
    }
    let contributions = added
        .iter()
        .map(|entry| contribute(entry, None))
        .collect::<Vec<_>>();
    let (settled, open) = settle(
        previous.settled.clone(),
        previous
            .open
            .iter()
            .cloned()
            .chain(contributions.iter().flatten().cloned())
            .collect(),
    );
    let baseline = previous
        .baseline
        .clone()
        .or_else(|| find_baseline(&bounds, &added));
    let messages = lead_with_system(
        settled
            .iter()
            .cloned()
            .chain(order_tool_results(&open))
            .collect(),
        &baseline,
    );
    let view = ContextView {
        head: bounds.head.clone(),
        entries: previous
            .view
            .entries
            .iter()
            .cloned()
            .chain(added.into_iter().map(|entry| entry.record))
            .collect(),
        contributions: previous
            .view
            .contributions
            .iter()
            .cloned()
            .chain(contributions)
            .collect(),
        messages,
    };
    ContextRange {
        bounds,
        entries,
        view,
        edited: previous.edited.clone(),
        settled,
        open,
        baseline,
    }
}
fn find_baseline(bounds: &ContextBounds, entries: &[ContextEntry]) -> Baseline {
    for entry in entries {
        if bounds
            .head
            .as_ref()
            .is_some_and(|head| entry.record.id <= head.id)
        {
            continue;
        }
        if entry.record.kind == *crate::SYSTEM_ENTRY.kind() {
            return Some(
                entry
                    .model
                    .iter()
                    .find(|message| role(message, "system"))
                    .cloned(),
            );
        }
        if bounds.head.is_none()
            && (entry.model.iter().any(|message| role(message, "system"))
                || entry
                    .edits
                    .iter()
                    .filter_map(|(_, messages)| messages.as_ref())
                    .flatten()
                    .any(|message| role(message, "system")))
        {
            return Some(None);
        }
    }
    None
}
fn lead_with_system(mut messages: Vec<Message>, baseline: &Baseline) -> Vec<Message> {
    let Some(index) = messages
        .iter()
        .position(|message| role(message, "system"))
        .filter(|index| *index > 0)
    else {
        return messages;
    };
    let is_baseline = baseline
        .as_ref()
        .and_then(Option::as_ref)
        .is_some_and(|baseline| Arc::ptr_eq(baseline, &messages[index]));
    if !is_baseline
        && messages[..index]
            .iter()
            .any(|message| !role(message, "user"))
    {
        return messages;
    }
    let system = messages.remove(index);
    messages.insert(0, system);
    messages
}
fn settle(mut settled: Vec<Message>, open: Vec<Message>) -> (Vec<Message>, Vec<Message>) {
    let Some(last) = open
        .iter()
        .rposition(|message| role(message, "assistant"))
        .filter(|last| *last > 0)
    else {
        return (settled, open);
    };
    settled.extend(order_tool_results(&open[..last]));
    (settled, open[last..].to_vec())
}
/// Order results by call order, synthesize missing receipts, and discard stray
/// results. Matching never crosses the following assistant message.
pub fn order_tool_results(messages: &[Message]) -> Vec<Message> {
    let mut ordered = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if role(message, "toolResult") {
            continue;
        }
        ordered.push(message.clone());
        if !role(message, "assistant") {
            continue;
        }
        let calls = wire::get(message, "content")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .filter(|item| {
                wire::get(item, "type").is_some_and(|kind| kind.string_equals("toolCall"))
            })
            .collect::<Vec<_>>();
        if calls.is_empty() {
            continue;
        }
        let mut results = HashMap::<JsonString, Message>::new();
        for candidate in messages[index + 1..]
            .iter()
            .take_while(|message| !role(message, "assistant"))
        {
            if role(candidate, "toolResult")
                && let Some(id) = wire::get(candidate, "toolCallId").and_then(JsonValue::as_string)
            {
                results
                    .entry(id.clone())
                    .or_insert_with(|| candidate.clone());
            }
        }
        for call in calls {
            let result = wire::get(call, "id")
                .and_then(JsonValue::as_string)
                .and_then(|id| results.get(id))
                .cloned()
                .unwrap_or_else(|| missing_result(call, message));
            ordered.push(result);
        }
    }
    ordered
}
fn missing_result(call: &JsonValue, assistant: &JsonValue) -> Message {
    let mut result = wire::object();
    for (key, value) in [
        ("role", JsonValue::from("toolResult")),
        (
            "toolCallId",
            wire::get(call, "id").cloned().unwrap_or(JsonValue::Null),
        ),
        (
            "toolName",
            wire::get(call, "name").cloned().unwrap_or(JsonValue::Null),
        ),
        (
            "content",
            wire::encode(&[
                serde_json::json!({"type":"text","text":"Tool result unavailable: history ends before this call completed."}),
            ]),
        ),
        ("isError", wire::encode(&true)),
        (
            "details",
            wire::encode(&serde_json::json!({"reason":"missing_result"})),
        ),
        (
            "timestamp",
            wire::get(assistant, "timestamp")
                .cloned()
                .unwrap_or(JsonValue::Null),
        ),
    ] {
        result.as_object_mut().unwrap().insert(key.into(), value);
    }
    Arc::new(result)
}
