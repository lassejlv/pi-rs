use super::*;
use pi_durable::harness::context::{self, ContextBounds};
pub(super) fn encode_view(view: &context::ContextView) -> JsonValue {
    // JSON alone loses aliasing. Preserve native Arc identity across the two
    // public message arrays, including equal messages from distinct entries.
    let positions = view
        .contributions
        .iter()
        .enumerate()
        .flat_map(|(entry, messages)| {
            messages
                .iter()
                .enumerate()
                .map(move |(message, value)| (Arc::as_ptr(value) as usize, (entry, message)))
        })
        .collect::<HashMap<_, _>>();
    let aliases = view
        .messages
        .iter()
        .map(|message| positions.get(&(Arc::as_ptr(message) as usize)).copied())
        .collect::<Vec<_>>();
    object([
        ("value", encode(view)),
        ("messageContributions", encode(&aliases)),
    ])
}
/// Values a host already holds, by id, so each crosses once and the host keeps one frozen object
/// per value, as upstream shares kept entries between views:
/// - entries by `(namespace, entry id)` (entries are immutable; the namespace is the Harness),
///   sent as `{"$def": id, "v": entry}` once and `{"$ref": id}` after;
/// - a contributed message that is one of its entry's `model` messages as `{"$m": [entry, index]}`;
/// - other messages (edits, synthesized results) by native identity, reported in `dropped` once
///   the native side let them go.
#[derive(Default)]
pub struct ContextRefs {
    next: u64,
    entries: HashMap<(u64, u64), u64>,
    messages: HashMap<usize, (std::sync::Weak<JsonValue>, u64)>,
    encodes: u64,
}
impl ContextRefs {
    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
    fn entry(&mut self, namespace: u64, entry: &pi_durable::EntryRecord) -> (JsonValue, u64) {
        let key = (namespace, encode(&entry.id).as_u64().unwrap_or_default());
        if let Some(id) = self.entries.get(&key) {
            return (object([("$ref", encode(id))]), *id);
        }
        let id = self.id();
        self.entries.insert(key, id);
        (object([("$def", encode(&id)), ("v", encode(entry))]), id)
    }
    fn message(&mut self, message: &Arc<JsonValue>) -> JsonValue {
        let key = Arc::as_ptr(message) as usize;
        if let Some((weak, id)) = self.messages.get(&key)
            && weak
                .upgrade()
                .is_some_and(|held| Arc::ptr_eq(&held, message))
        {
            return object([("$ref", encode(id))]);
        }
        let id = self.id();
        self.messages.insert(key, (Arc::downgrade(message), id));
        object([("$def", encode(&id)), ("v", (**message).clone())])
    }
    fn sweep(&mut self) -> Vec<u64> {
        let mut dropped = Vec::new();
        self.messages.retain(|_, (weak, id)| {
            let alive = weak.strong_count() > 0;
            if !alive {
                dropped.push(*id);
            }
            alive
        });
        dropped
    }
    /// Forget a namespace's entries (its Harness closed); the host forgets them too.
    pub(super) fn forget(&mut self, namespace: u64) {
        self.entries.retain(|(ns, _), _| *ns != namespace);
    }
}
/// `encode_view` with values the host holds sent as references (see [`ContextRefs`]).
pub(super) fn encode_view_refs(
    view: &context::ContextView,
    namespace: u64,
    refs: &Mutex<ContextRefs>,
) -> JsonValue {
    let mut refs = refs.lock();
    let head = view
        .head
        .as_ref()
        .map(|entry| refs.entry(namespace, entry).0);
    let mut entry_ids = Vec::with_capacity(view.entries.len());
    let mut entries = Vec::with_capacity(view.entries.len());
    for entry in &view.entries {
        let (value, id) = refs.entry(namespace, entry);
        entries.push(value);
        entry_ids.push(id);
    }
    let aligned = view.contributions.len() == view.entries.len();
    let mut seen: HashMap<usize, JsonValue> = HashMap::new();
    let mut contributions = Vec::with_capacity(view.contributions.len());
    for (index, messages) in view.contributions.iter().enumerate() {
        let model = aligned
            .then(|| view.entries[index].model.as_deref())
            .flatten();
        let mut encoded = Vec::with_capacity(messages.len());
        for message in messages {
            let value = match model.and_then(|model| model.iter().position(|m| m == &**message)) {
                Some(position) => object([("$m", encode(&[entry_ids[index], position as u64]))]),
                None => refs.message(message),
            };
            seen.insert(Arc::as_ptr(message) as usize, value.clone());
            encoded.push(value);
        }
        contributions.push(JsonValue::Array(Arc::new(encoded)));
    }
    let messages: Vec<JsonValue> = view
        .messages
        .iter()
        .map(|m| {
            seen.get(&(Arc::as_ptr(m) as usize))
                .cloned()
                .unwrap_or_else(|| refs.message(m))
        })
        .collect();
    refs.encodes += 1;
    let dropped = if refs.encodes.is_multiple_of(64) {
        refs.sweep()
    } else {
        Vec::new()
    };
    let mut value = object([
        ("entries", JsonValue::Array(Arc::new(entries))),
        ("contributions", JsonValue::Array(Arc::new(contributions))),
        ("messages", JsonValue::Array(Arc::new(messages))),
    ]);
    if let Some(head) = head {
        value.as_object_mut().unwrap().insert("head".into(), head);
    }
    object([("refs", value), ("dropped", encode(&dropped))])
}
impl Hub {
    pub fn context_request(
        self: Arc<Self>,
        method: &str,
        request: JsonValue,
        context: Context,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        if method == "context.release" {
            self.ranges.lock().remove(&field::<u64>(&request, "range")?);
            return Ok(Box::pin(async { Ok(None) }));
        }
        self.context_reads.fetch_add(1, Ordering::Relaxed);
        let storage = remote_storage::RemoteStorage {
            hub: self.clone(),
            handle: field(&request, "storage")?,
        };
        let conversation = field(&request, "conversation")?;
        Ok(match method {
            "context.capture" => Box::pin(async move {
                Ok(context::capture_context_bounds(
                    &storage,
                    conversation,
                    &context,
                    field(&request, "at")?,
                )
                .await?
                .map(|bounds| encode(&bounds)))
            }),
            "context.derive" => Box::pin(async move {
                Ok(Some(encode_view(
                    &context::derive_context(
                        &storage,
                        conversation,
                        field(&request, "bounds")?,
                        &context,
                    )
                    .await?,
                )))
            }),
            "context.active" => Box::pin(async move {
                Ok(Some(encode(
                    &context::active_entries(
                        &storage,
                        conversation,
                        field(&request, "bounds")?,
                        &context,
                    )
                    .await?,
                )))
            }),
            "context.range" => {
                let previous: Option<u64> = field(&request, "previous")?;
                let previous = previous.and_then(|id| self.ranges.lock().get(&id).cloned());
                let bounds: Option<ContextBounds> = field(&request, "bounds")?;
                Box::pin(async move {
                    let read =
                        context::read_range(&storage, conversation, bounds, &context, previous)
                            .await?;
                    let range = read.range.map(|range| {
                        let id = self.next.fetch_add(1, Ordering::Relaxed);
                        let bounds = encode(&range.bounds);
                        self.ranges.lock().insert(id, range);
                        object([("handle", encode(&id)), ("bounds", bounds)])
                    });
                    Ok(Some(object([
                        ("view", encode_view(&read.view)),
                        ("range", range.unwrap_or(JsonValue::Null)),
                    ])))
                })
            }
            other => {
                return Err(Error::message(format!(
                    "Unknown context bridge method {other}"
                )));
            }
        })
    }
}
