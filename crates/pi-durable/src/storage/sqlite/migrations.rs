use super::schema::SqliteSchema;
pub use pi_chord::sqlite::SqliteMigration;
use pi_chord::sqlite::{SqliteDatabase, SqliteFuture};

pub const CURRENT_SQLITE_SCHEMA_VERSION: u32 = 2;
pub static SQLITE_MIGRATIONS: std::sync::LazyLock<Vec<SqliteMigration>> =
    std::sync::LazyLock::new(|| create_sqlite_migrations(&SqliteSchema::default()));

fn task_indexes(schema: &SqliteSchema) -> Vec<String> {
    let t = &schema.tables.tasks;
    let i = &schema.indexes;
    [
        (&i.tasks_by_status, "status"),
        (&i.tasks_by_conversation, "conversation_id"),
        (&i.tasks_by_kind, "kind"),
        (&i.tasks_by_abort_requested, "abort_requested"),
        (&i.tasks_by_background, "background"),
    ]
    .into_iter()
    .map(|(index, column)| format!("CREATE INDEX {index} ON {t} ({column}, id)"))
    .collect()
}
fn task_table(table: &str, sleeping: bool) -> String {
    let statuses = if sleeping {
        "'pending', 'running', 'waiting', 'sleeping', 'completing', 'terminal'"
    } else {
        "'pending', 'running', 'waiting', 'completing', 'terminal'"
    };
    format!(
        "CREATE TABLE {table} (
        id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL, kind TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ({statuses})),
        abort_requested INTEGER NOT NULL CHECK (abort_requested IN (0, 1)),
        background INTEGER NOT NULL CHECK (background IN (0, 1)),
        record TEXT NOT NULL CHECK (json_valid(record))
    ) STRICT"
    )
}
pub fn create_sqlite_migrations(schema: &SqliteSchema) -> Vec<SqliteMigration> {
    let t = &schema.tables;
    let i = &schema.indexes;
    // TEXT preserves the exhausted ID sentinel beyond JS's safe integer range.
    let mut initial = vec![
        format!(
            "CREATE TABLE {} (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), next_id TEXT NOT NULL, next_seq INTEGER NOT NULL) STRICT",
            t.durable_metadata
        ),
        format!(
            "INSERT INTO {} (singleton, next_id, next_seq) VALUES (1, '2', 1)",
            t.durable_metadata
        ),
        format!(
            "CREATE TABLE {} (id INTEGER PRIMARY KEY, record_type TEXT NOT NULL CHECK (record_type IN ('conversation', 'entry', 'task', 'submission', 'document'))) STRICT",
            t.record_ids
        ),
        format!(
            "CREATE TABLE {} (id INTEGER PRIMARY KEY, owner_conversation_id INTEGER, owner_task_id INTEGER, record TEXT NOT NULL CHECK (json_valid(record))) STRICT",
            t.conversations
        ),
        format!(
            "CREATE INDEX {} ON {} (owner_conversation_id, id)",
            i.conversations_by_owner_conversation, t.conversations
        ),
        format!(
            "CREATE INDEX {} ON {} (owner_task_id, id)",
            i.conversations_by_owner_task, t.conversations
        ),
        format!(
            "CREATE TABLE {} (id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL, head INTEGER, commit_seq INTEGER NOT NULL, record TEXT NOT NULL CHECK (json_valid(record))) STRICT",
            t.entries
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, id DESC)",
            i.entries_by_conversation, t.entries
        ),
        format!(
            "CREATE INDEX {} ON {} (conversation_id, id DESC) WHERE head IS NOT NULL",
            i.entry_heads_by_conversation, t.entries
        ),
        task_table(&t.tasks, false),
    ];
    initial.extend(task_indexes(schema));
    initial.extend([
        format!("CREATE TABLE {} (id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL, request_id TEXT, status TEXT NOT NULL CHECK (status IN ('queued', 'placed', 'done', 'unanswered')), record TEXT NOT NULL CHECK (json_valid(record))) STRICT", t.submissions),
        format!("CREATE INDEX {} ON {} (conversation_id, request_id)", i.submissions_by_request, t.submissions),
        format!("CREATE INDEX {} ON {} (conversation_id, id)", i.submissions_by_conversation, t.submissions),
        format!("CREATE INDEX {} ON {} (status, id)", i.submissions_by_status, t.submissions),
        format!("CREATE TABLE {} (id INTEGER PRIMARY KEY, kind TEXT NOT NULL, family INTEGER NOT NULL CHECK (family IN (0, 1)), key_value TEXT NOT NULL, scope_kind TEXT NOT NULL CHECK (scope_kind IN ('session', 'conversation', 'task')), owner_id INTEGER NOT NULL, created_at INTEGER NOT NULL, retired_at INTEGER, record TEXT NOT NULL CHECK (json_valid(record))) STRICT", t.documents),
        format!("CREATE INDEX {} ON {} (kind, scope_kind, owner_id, family, key_value, created_at DESC, retired_at)", i.documents_by_address, t.documents),
        format!("CREATE INDEX {} ON {} (scope_kind, owner_id, id)", i.documents_by_scope, t.documents),
        format!("CREATE INDEX {} ON {} (scope_kind, owner_id, kind, id)", i.documents_by_scope_kind, t.documents),
        format!("CREATE TABLE {} (document_id INTEGER NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('base', 'delta')), version INTEGER NOT NULL, content TEXT NOT NULL CHECK (json_valid(content)), PRIMARY KEY (document_id, seq)) STRICT", t.document_revisions),
        format!("CREATE INDEX {} ON {} (document_id, kind, seq DESC)", i.document_revisions_by_kind, t.document_revisions),
    ]);
    let rebuilt = format!("{}_v2", t.tasks);
    let mut sleeping = vec![
        task_table(&rebuilt, true),
        format!(
            "INSERT INTO {rebuilt} (id, conversation_id, kind, status, abort_requested, background, record) SELECT id, conversation_id, kind, status, abort_requested, background, record FROM {}",
            t.tasks
        ),
        format!("DROP TABLE {}", t.tasks),
        format!("ALTER TABLE {rebuilt} RENAME TO {}", t.tasks),
    ];
    sleeping.extend(task_indexes(schema));
    vec![
        SqliteMigration {
            version: 1,
            statements: initial,
        },
        SqliteMigration {
            version: 2,
            statements: sleeping,
        },
    ]
}
pub fn apply_sqlite_migrations(
    database: &dyn SqliteDatabase,
    migrations: Option<&[SqliteMigration]>,
) -> SqliteFuture<()> {
    apply_sqlite_migrations_for_schema(database, &SqliteSchema::default(), migrations)
}
pub fn apply_sqlite_migrations_for_schema(
    database: &dyn SqliteDatabase,
    schema: &SqliteSchema,
    migrations: Option<&[SqliteMigration]>,
) -> SqliteFuture<()> {
    pi_chord::sqlite::apply_sqlite_migrations(
        database,
        migrations.unwrap_or(&create_sqlite_migrations(schema)),
        &schema.tables.durable_schema,
    )
}
