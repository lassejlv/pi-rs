use pi_chord::sqlite::SqliteResult;
pub use pi_chord::sqlite::validate_sqlite_schema_prefix;
pub static DEFAULT_SQLITE_SCHEMA: std::sync::LazyLock<SqliteSchema> =
    std::sync::LazyLock::new(SqliteSchema::default);

// Names are immutable migration history. Renames must add new identifiers.
macro_rules! identifiers {
    ($name:ident { $($field:ident),* $(,)? }) => {
        #[derive(Clone, Debug, serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        pub struct $name { $(pub $field: String),* }
        impl $name {
            fn prefixed(prefix: &str) -> Self {
                Self { $($field: format!("{prefix}{}", stringify!($field))),* }
            }
        }
    };
}
identifiers!(SqliteTables {
    durable_schema,
    durable_metadata,
    record_ids,
    conversations,
    entries,
    tasks,
    submissions,
    documents,
    document_revisions,
});
identifiers!(SqliteIndexes {
    conversations_by_owner_conversation,
    conversations_by_owner_task,
    entries_by_conversation,
    entry_heads_by_conversation,
    tasks_by_status,
    tasks_by_conversation,
    tasks_by_kind,
    tasks_by_abort_requested,
    tasks_by_background,
    submissions_by_request,
    submissions_by_conversation,
    submissions_by_status,
    documents_by_address,
    documents_by_scope,
    documents_by_scope_kind,
    document_revisions_by_kind,
});
#[derive(Clone, Debug, serde::Serialize)]
pub struct SqliteSchema {
    pub tables: SqliteTables,
    pub indexes: SqliteIndexes,
}
pub fn create_sqlite_schema(prefix: Option<&str>) -> SqliteResult<SqliteSchema> {
    if let Some(prefix) = prefix {
        validate_sqlite_schema_prefix(prefix)?;
    }
    let prefix = prefix.unwrap_or("");
    Ok(SqliteSchema {
        tables: SqliteTables::prefixed(prefix),
        indexes: SqliteIndexes::prefixed(prefix),
    })
}
impl Default for SqliteSchema {
    fn default() -> Self {
        create_sqlite_schema(None).expect("default SQLite identifiers")
    }
}
