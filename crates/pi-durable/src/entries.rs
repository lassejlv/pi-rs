use crate::{
    ConversationId, EntryDraft, EntryRecord, Error, Result, Transaction, storage::StorageFuture,
};
use pi_chord::{JsonString, JsonValue};
use std::marker::PhantomData;

#[derive(Debug)]
pub struct Entry<D = JsonValue> {
    kind: JsonString,
    marker: PhantomData<fn() -> D>,
}
// Manual: an entry token is a kind, so it clones whatever its data type.
impl<D> Clone for Entry<D> {
    fn clone(&self) -> Self {
        Self {
            kind: self.kind.clone(),
            marker: PhantomData,
        }
    }
}
impl<D> Entry<D> {
    pub fn kind(&self) -> &JsonString {
        &self.kind
    }
    pub fn is(&self, entry: Option<&EntryRecord>) -> bool {
        entry.is_some_and(|entry| entry.kind == self.kind)
    }
}
impl<D: serde::Serialize + serde::de::DeserializeOwned> Entry<D> {
    /// Append an entry of this kind whose `data` is `data`; `model` is the optional context contribution.
    pub fn append(
        &self,
        tx: &Transaction,
        conversation: ConversationId,
        data: &D,
        model: Option<Vec<JsonValue>>,
    ) -> StorageFuture<EntryRecord> {
        let mut draft = EntryDraft::new(self.kind.clone());
        draft.model = model;
        match pi_chord::json::to_value(data) {
            Ok(data) => draft.data = Some(data),
            Err(error) => return Box::pin(std::future::ready(Err(Error::message(error)))),
        }
        tx.append_entry(conversation, draft)
    }
    /// The typed `data` of an entry of this kind; `None` for another kind or when `data` is absent.
    pub fn data(&self, entry: &EntryRecord) -> Result<Option<D>> {
        match (&entry.data, self.is(Some(entry))) {
            (Some(data), true) => crate::wire::decode(data).map(Some).map_err(Error::message),
            _ => Ok(None),
        }
    }
}
pub fn define_entry<D>(kind: impl Into<JsonString>) -> Result<Entry<D>> {
    let kind = kind.into();
    if kind.units().is_empty() {
        return Err(Error::message("Entry kind must be a non-empty string"));
    }
    Ok(Entry {
        kind,
        marker: PhantomData,
    })
}
pub static USER_ENTRY: std::sync::LazyLock<Entry> =
    std::sync::LazyLock::new(|| define_entry("pi.user").unwrap());
pub static ASSISTANT_ENTRY: std::sync::LazyLock<Entry> =
    std::sync::LazyLock::new(|| define_entry("pi.assistant").unwrap());
pub static SYSTEM_ENTRY: std::sync::LazyLock<Entry> =
    std::sync::LazyLock::new(|| define_entry("pi.system").unwrap());
pub static TOOL_RESULT_ENTRY: std::sync::LazyLock<Entry> =
    std::sync::LazyLock::new(|| define_entry("pi.tool-result").unwrap());
pub static RESET_ENTRY: std::sync::LazyLock<Entry> =
    std::sync::LazyLock::new(|| define_entry("pi.reset").unwrap());
pub static COMPACTION_ENTRY: std::sync::LazyLock<Entry> =
    std::sync::LazyLock::new(|| define_entry("pi.compaction").unwrap());
