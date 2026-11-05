use crate::{storage::StorageFuture, *};
use std::{collections::HashSet, sync::Arc};

pub(crate) fn prepare_fork_copies(
    storage: Arc<dyn Storage>,
    parent: ConversationId,
    at: EntryId,
    child: ConversationId,
    context: Context,
) -> StorageFuture<Vec<(DocumentCreate, DocumentCopySource)>> {
    Box::pin(async move {
        let entry = storage
            .entry_in(parent, at, &context)
            .await?
            .ok_or_else(|| {
                Error::message(format!(
                    "Entry {at} is not visible from conversation {parent}"
                ))
            })?;
        let mut copies = Vec::new();
        let mut addresses = HashSet::new();
        for (conversation_id, point, historical) in [
            (
                entry.entry.conversation_id,
                DocumentPoint::At(entry.commit_seq),
                true,
            ),
            (parent, DocumentPoint::Current, false),
        ] {
            let query = DocumentQuery {
                scope: DocumentScope::Conversation { conversation_id },
                at: point,
                kind: None,
            };
            let mut cursor = None;
            loop {
                let page = storage
                    .scan_documents(&query, 256, cursor.as_ref(), &context)
                    .await?;
                for document in page.items {
                    let DocumentSemantics::Conversation { history, .. } =
                        &document.create.semantics
                    else {
                        continue;
                    };
                    let selected = matches!(
                        history,
                        ConversationHistory::Rewindable {
                            fork: ForkPolicy::AsOf
                        }
                    ) && historical
                        || matches!(
                            history,
                            ConversationHistory::Latest {
                                fork: LatestFork::Current
                            } | ConversationHistory::Rewindable {
                                fork: ForkPolicy::Current
                            }
                        ) && !historical;
                    if !selected {
                        continue;
                    }
                    let record = DocumentCreate {
                        id: DocumentId::from_raw(storage.mint_id().await?),
                        kind: document.create.kind.clone(),
                        key: document.create.key.clone(),
                        semantics: DocumentSemantics::Conversation {
                            conversation_id: child,
                            history: history.clone(),
                        },
                    };
                    if !addresses.insert(address_id(&record.address())) {
                        let member = record.key.as_ref().map_or_else(
                            || record.kind.to_string(),
                            |key| format!("{}/{key}", record.kind),
                        );
                        return Err(Error::message(format!(
                            "Fork selects multiple source documents for {member}"
                        )));
                    }
                    copies.push((
                        record,
                        DocumentCopySource {
                            id: document.id(),
                            at: point,
                        },
                    ));
                }
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        Ok(copies)
    })
}
