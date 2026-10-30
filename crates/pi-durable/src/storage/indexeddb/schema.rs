//! The upstream `@pi/durable` IndexedDB schema (database version 1). Store,
//! index and field names are the on-disk contract shared with the TypeScript
//! implementation; changing any of them breaks interchange.
use super::key::KeyPath;
use crate::JsonValue;

pub const DATABASE_VERSION: u32 = 1;
pub const MAX_KEY: i64 = crate::MAX_SAFE_INTEGER;
pub const META_KEY: &str = "state";

pub const META: &str = "meta";
pub const RECORD_IDS: &str = "record_ids";
pub const CONVERSATIONS: &str = "conversations";
pub const ENTRIES: &str = "entries";
pub const TASKS: &str = "tasks";
pub const SUBMISSIONS: &str = "submissions";
pub const DOCUMENTS: &str = "documents";
pub const DOCUMENT_REVISIONS: &str = "document_revisions";
pub const ALL_STORES: &[&str] = &[
    META,
    RECORD_IDS,
    CONVERSATIONS,
    ENTRIES,
    TASKS,
    SUBMISSIONS,
    DOCUMENTS,
    DOCUMENT_REVISIONS,
];

pub const CONVERSATIONS_BY_OWNER_CONVERSATION: &str = "conversations_by_owner_conversation";
pub const CONVERSATIONS_BY_OWNER_TASK: &str = "conversations_by_owner_task";
pub const ENTRIES_BY_CONVERSATION: &str = "entries_by_conversation";
pub const ENTRY_HEADS_BY_CONVERSATION: &str = "entry_heads_by_conversation";
pub const TASKS_BY_STATUS: &str = "tasks_by_status";
pub const TASKS_BY_CONVERSATION: &str = "tasks_by_conversation";
pub const TASKS_BY_KIND: &str = "tasks_by_kind";
pub const TASKS_BY_ABORT_REQUESTED: &str = "tasks_by_abort_requested";
pub const TASKS_BY_BACKGROUND: &str = "tasks_by_background";
pub const SUBMISSIONS_BY_REQUEST: &str = "submissions_by_request";
pub const SUBMISSIONS_BY_CONVERSATION: &str = "submissions_by_conversation";
pub const SUBMISSIONS_BY_STATUS: &str = "submissions_by_status";
pub const DOCUMENTS_BY_ADDRESS: &str = "documents_by_address";
pub const DOCUMENTS_BY_SCOPE: &str = "documents_by_scope";
pub const DOCUMENTS_BY_SCOPE_KIND: &str = "documents_by_scope_kind";
pub const DOCUMENT_REVISIONS_BY_KIND: &str = "document_revisions_by_kind";

#[derive(Debug)]
pub struct IndexSpec {
    pub name: &'static str,
    pub key_path: KeyPath,
}
#[derive(Debug)]
pub struct StoreSpec {
    pub name: &'static str,
    pub key_path: KeyPath,
    pub indexes: &'static [IndexSpec],
}
const fn index(name: &'static str, key_path: &'static [&'static str]) -> IndexSpec {
    IndexSpec {
        name,
        key_path: KeyPath::Compound(key_path),
    }
}
const fn by_id(name: &'static str, indexes: &'static [IndexSpec]) -> StoreSpec {
    StoreSpec {
        name,
        key_path: KeyPath::Single("id"),
        indexes,
    }
}

/// `createSchema()`, in creation order. None of the indexes is unique or multi-entry.
pub const SCHEMA: &[StoreSpec] = &[
    StoreSpec {
        name: META,
        key_path: KeyPath::Single("key"),
        indexes: &[],
    },
    by_id(RECORD_IDS, &[]),
    by_id(
        CONVERSATIONS,
        &[
            index(
                CONVERSATIONS_BY_OWNER_CONVERSATION,
                &["ownerConversationId", "id"],
            ),
            index(CONVERSATIONS_BY_OWNER_TASK, &["ownerTaskId", "id"]),
        ],
    ),
    by_id(
        ENTRIES,
        &[
            index(ENTRIES_BY_CONVERSATION, &["conversationId", "id"]),
            index(ENTRY_HEADS_BY_CONVERSATION, &["headConversationId", "id"]),
        ],
    ),
    by_id(
        TASKS,
        &[
            index(TASKS_BY_STATUS, &["status", "id"]),
            index(TASKS_BY_CONVERSATION, &["conversationId", "id"]),
            index(TASKS_BY_KIND, &["kind", "id"]),
            index(TASKS_BY_ABORT_REQUESTED, &["abortRequested", "id"]),
            index(TASKS_BY_BACKGROUND, &["background", "id"]),
        ],
    ),
    by_id(
        SUBMISSIONS,
        &[
            index(SUBMISSIONS_BY_REQUEST, &["conversationId", "requestId"]),
            index(SUBMISSIONS_BY_CONVERSATION, &["conversationId", "id"]),
            index(SUBMISSIONS_BY_STATUS, &["status", "id"]),
        ],
    ),
    by_id(
        DOCUMENTS,
        &[
            index(
                DOCUMENTS_BY_ADDRESS,
                &[
                    "kind",
                    "scopeKind",
                    "ownerId",
                    "family",
                    "keyValue",
                    "createdAt",
                    "id",
                ],
            ),
            index(DOCUMENTS_BY_SCOPE, &["scopeKind", "ownerId", "id"]),
            index(
                DOCUMENTS_BY_SCOPE_KIND,
                &["scopeKind", "ownerId", "kind", "id"],
            ),
        ],
    ),
    StoreSpec {
        name: DOCUMENT_REVISIONS,
        key_path: KeyPath::Compound(&["documentId", "seq"]),
        indexes: &[index(
            DOCUMENT_REVISIONS_BY_KIND,
            &["documentId", "kind", "seq"],
        )],
    },
];

pub fn store(name: &str) -> Option<&'static StoreSpec> {
    SCHEMA.iter().find(|store| store.name == name)
}

/// The row `createSchema()` adds to `meta` when the database is first created.
pub fn initial_meta() -> JsonValue {
    meta_row(2, 1)
}
pub fn meta_row(next_id: i64, next_seq: i64) -> JsonValue {
    let mut row = crate::wire::object();
    crate::wire::put(&mut row, "key", META_KEY);
    crate::wire::put(&mut row, "nextId", &next_id);
    crate::wire::put(&mut row, "nextSeq", &next_seq);
    row
}
