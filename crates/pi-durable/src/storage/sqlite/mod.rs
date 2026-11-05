//! Indexed, transactional storage compatible with pi-durable's SQLite files.
use super::{
    Adopt, Storage, StorageFuture, StoredEntry,
    line::eager,
    ready,
    scan::{self, ScanStart},
};
use crate::{Context, Error, JsonString, Result, types::*};
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_chord::delta::{Op, apply_immutable_batches};
use serde::{Serialize, de::DeserializeOwned};
use std::{collections::HashMap, sync::Arc};
#[cfg(all(target_arch = "wasm32", feature = "cloudflare"))]
pub mod cloudflare_js;
pub mod migrations;
pub mod schema;
pub use migrations::*;
#[cfg(not(target_arch = "wasm32"))]
pub use pi_chord::sqlite::{
    NativeSqliteDatabase, NativeSqliteDatabaseOptions, open_native_sqlite_database,
};
pub use pi_chord::sqlite::{
    SqliteDatabase, SqliteDatabaseExt, SqliteError, SqliteExecutor, SqliteFuture, SqliteRow,
    SqliteValue,
    cloudflare::{DurableObjectSqliteDatabase, DurableObjectSqliteStorage},
};
pub use schema::*;

#[derive(Clone, Debug, Default)]
pub struct SqliteStorageOptions {
    pub prefix: Option<String>,
}
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug, Default)]
pub struct NativeSqliteStorageOptions {
    pub prefix: Option<String>,
    pub database: NativeSqliteDatabaseOptions,
}
struct State {
    next_id: i64,
    closed: bool,
    admitted_reads: usize,
    closing: Option<Shared<BoxFuture<'static, Result<()>>>>,
    publication: Shared<BoxFuture<'static, ()>>,
}
struct Inner {
    database: Arc<dyn SqliteDatabase>,
    schema: SqliteSchema,
    state: Mutex<State>,
    drained: tokio::sync::Notify,
}
#[derive(Clone)]
pub struct SqliteStorage {
    inner: Arc<Inner>,
}
struct ReadGuard(Arc<Inner>);
impl Drop for ReadGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock();
        state.admitted_reads -= 1;
        if state.admitted_reads == 0 {
            self.0.drained.notify_one();
        }
    }
}
struct Publication(Option<tokio::sync::oneshot::Sender<()>>);
impl Drop for Publication {
    fn drop(&mut self) {
        if let Some(send) = self.0.take() {
            let _ = send.send(());
        }
    }
}
fn assert_open(state: &State) -> Result<()> {
    if state.closed {
        Err(Error::message("SqliteStorage is closed"))
    } else {
        Ok(())
    }
}
fn encode(value: &(impl Serialize + ?Sized)) -> Result<String> {
    Ok(pi_chord::json::to_value(value)
        .map_err(Error::message)?
        .to_string())
}
fn text(row: &SqliteRow, key: &str) -> Result<String> {
    match row.get(key) {
        Some(SqliteValue::Text(value)) => Ok(value.clone()),
        _ => Err(Error::message(format!("Invalid SQLite {key}"))),
    }
}
fn integer(row: &SqliteRow, key: &str) -> Result<i64> {
    match row.get(key) {
        Some(SqliteValue::Integer(value)) => Ok(*value),
        Some(SqliteValue::Real(value))
            if value.is_finite()
                && value.fract() == 0.0
                && value.abs() <= MAX_SAFE_INTEGER as f64 =>
        {
            Ok(*value as i64)
        }
        _ => Err(Error::message(format!("Invalid SQLite {key}"))),
    }
}
fn record<T: DeserializeOwned>(row: &SqliteRow) -> Result<T> {
    Ok(serde_json::from_str(&text(row, "record")?)?)
}
fn sql_text(value: impl Into<String>) -> SqliteValue {
    SqliteValue::Text(value.into())
}
fn indexed(value: &JsonString) -> SqliteValue {
    sql_text(crate::wire::encode(value).to_string())
}
fn opt_id(value: Option<i64>) -> SqliteValue {
    value.map_or(SqliteValue::Null, SqliteValue::Integer)
}
fn token(value: &impl Serialize) -> SqliteValue {
    sql_text(
        crate::wire::encode(value)
            .to_utf8()
            .expect("enum string discriminator"),
    )
}
fn scope_parts(scope: &DocumentScope) -> (&'static str, i64) {
    match scope {
        DocumentScope::Session => ("session", 0),
        DocumentScope::Conversation { conversation_id } => ("conversation", conversation_id.get()),
        DocumentScope::Task { task_id } => ("task", task_id.get()),
    }
}
fn address_parts(address: &DocumentAddress) -> Vec<SqliteValue> {
    let (scope, owner) = scope_parts(&address.scope);
    vec![
        indexed(&address.kind),
        scope.into(),
        owner.into(),
        i64::from(address.key.is_some()).into(),
        address
            .key
            .as_ref()
            .map_or_else(|| sql_text("\"\""), indexed),
    ]
}
fn page<T>(mut items: Vec<T>, limit: usize, order: ScanOrder, id: impl Fn(&T) -> i64) -> Page<T> {
    let extra = items.len() > limit;
    items.truncate(limit);
    let next = extra
        .then(|| items.last().map(|last| scan::next_cursor(id(last), order)))
        .flatten();
    Page { items, next }
}
struct ScanSql {
    conditions: Vec<String>,
    params: Vec<SqliteValue>,
    order: &'static str,
}
impl ScanSql {
    fn new(start: &ScanStart) -> Self {
        match start.order {
            ScanOrder::Ascending => Self {
                conditions: vec!["id > ?".into()],
                params: vec![start.after.unwrap_or(-1).into()],
                order: "ASC",
            },
            ScanOrder::Descending => Self {
                conditions: vec!["id < ?".into()],
                params: vec![start.after.unwrap_or(MAX_SAFE_INTEGER).into()],
                order: "DESC",
            },
        }
    }
    fn and(&mut self, condition: &str, value: SqliteValue) {
        self.conditions.push(condition.into());
        self.params.push(value);
    }
    fn all(
        &mut self,
        db: &dyn SqliteExecutor,
        table: &str,
        limit: usize,
    ) -> SqliteFuture<Vec<SqliteRow>> {
        self.params.push((limit.saturating_add(1) as i64).into());
        db.all(
            &format!(
                "SELECT record FROM {table} WHERE {} ORDER BY id {} LIMIT ?",
                self.conditions.join(" AND "),
                self.order
            ),
            &self.params,
        )
    }
}
impl SqliteStorage {
    /// Takes ownership of the facade; initialization failure closes it as well.
    pub fn open(
        database: Arc<dyn SqliteDatabase>,
        options: SqliteStorageOptions,
    ) -> StorageFuture<Self> {
        let schema = create_sqlite_schema(options.prefix.as_deref());
        let migrations = schema
            .as_ref()
            .ok()
            .map(|schema| apply_sqlite_migrations_for_schema(database.as_ref(), schema, None));
        eager(async move {
            let opened = async {
                let schema = schema?;
                migrations.expect("valid schema migrations").await?;
                let metadata = database
                    .get(
                        &format!(
                            "SELECT next_id FROM {} WHERE singleton = 1",
                            schema.tables.durable_metadata
                        ),
                        &[],
                    )
                    .await?
                    .ok_or_else(|| Error::message("Durable SQLite metadata is missing"))?;
                let next_id = text(&metadata, "next_id")?
                    .parse::<i64>()
                    .map_err(|e| Error::message(e.to_string()))?;
                Ok((schema, next_id))
            }
            .await;
            match opened {
                Ok((schema, next_id)) => Ok(Self {
                    inner: Arc::new(Inner {
                        database,
                        schema,
                        state: Mutex::new(State {
                            next_id,
                            closed: false,
                            admitted_reads: 0,
                            closing: None,
                            publication: async {}.boxed().shared(),
                        }),
                        drained: tokio::sync::Notify::new(),
                    }),
                }),
                Err(error) => {
                    let _ = database.close().await;
                    Err(error)
                }
            }
        })
    }
    pub fn schema(&self) -> &SqliteSchema {
        &self.inner.schema
    }
    fn ensure_open(&self) -> Result<()> {
        assert_open(&self.inner.state.lock())
    }
    fn get_record<T: DeserializeOwned + Send + 'static>(
        &self,
        table: &str,
        condition: &str,
        params: &[SqliteValue],
    ) -> StorageFuture<Option<T>> {
        let state = self.inner.state.lock();
        if let Err(error) = assert_open(&state) {
            return ready(Err(error));
        }
        let read = self.inner.database.get(
            &format!("SELECT record FROM {table} WHERE {condition}"),
            params,
        );
        eager(async move { read.await?.as_ref().map(record).transpose() })
    }
    fn scan<T: DeserializeOwned + Send + 'static>(
        &self,
        table: &str,
        mut sql: ScanSql,
        limit: usize,
        order: ScanOrder,
        id: impl Fn(&T) -> i64 + Send + 'static,
    ) -> StorageFuture<Page<T>> {
        let state = self.inner.state.lock();
        if let Err(error) = assert_open(&state) {
            return ready(Err(error));
        }
        let read = sql.all(self.inner.database.as_ref(), table, limit);
        eager(async move {
            Ok(page(
                read.await?.iter().map(record).collect::<Result<Vec<T>>>()?,
                limit,
                order,
                id,
            ))
        })
    }
    fn entry_read(
        &self,
        conversation: Option<ConversationId>,
        id: EntryId,
    ) -> StorageFuture<Option<StoredEntry>> {
        let mut state = self.inner.state.lock();
        if let Err(error) = assert_open(&state) {
            return ready(Err(error));
        }
        state.admitted_reads += 1;
        let guard = ReadGuard(self.inner.clone());
        let inner = self.inner.clone();
        let first = match conversation {
            Some(conversation) => inner.database.get(
                &format!(
                    "SELECT record FROM {} WHERE id = ?",
                    inner.schema.tables.conversations
                ),
                &[conversation.get().into()],
            ),
            None => inner.database.get(
                &format!(
                    "SELECT record, commit_seq FROM {} WHERE id = ?",
                    inner.schema.tables.entries
                ),
                &[id.get().into()],
            ),
        };
        eager(async move {
            let _guard = guard;
            let first = first.await?;
            let mut conv = match conversation {
                Some(conversation) => Some(required_conversation(first.clone(), conversation)?),
                None => None,
            };
            let row = if conversation.is_some() {
                inner
                    .database
                    .get(
                        &format!(
                            "SELECT record, commit_seq FROM {} WHERE id = ?",
                            inner.schema.tables.entries
                        ),
                        &[id.get().into()],
                    )
                    .await?
            } else {
                first
            };
            let Some(row) = row else {
                return Ok(None);
            };
            let entry: EntryRecord = record(&row)?;
            let mut upper = MAX_SAFE_INTEGER;
            while let Some(current) = conv {
                if current.id == entry.conversation_id {
                    break;
                }
                let Some(parent) = current.parent else {
                    return Ok(None);
                };
                upper = upper.min(parent.at.get());
                conv = Some(
                    read_conversation(
                        inner.database.as_ref(),
                        &inner.schema,
                        parent.conversation_id,
                    )
                    .await?,
                );
            }
            if id.get() > upper {
                return Ok(None);
            }
            Ok(Some(StoredEntry {
                entry,
                commit_seq: Seq::from_raw(integer(&row, "commit_seq")?),
            }))
        })
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub fn open_native_sqlite_storage(
    path: impl AsRef<std::path::Path>,
    options: NativeSqliteStorageOptions,
) -> StorageFuture<SqliteStorage> {
    let valid = create_sqlite_schema(options.prefix.as_deref());
    let path = path.as_ref().to_owned();
    eager(async move {
        valid?; // Validate before the driver can create directories or a file.
        let database = open_native_sqlite_database(path, options.database).await?;
        SqliteStorage::open(
            Arc::new(database),
            SqliteStorageOptions {
                prefix: options.prefix,
            },
        )
        .await
    })
}
pub fn open_durable_object_sqlite_storage(
    storage: Arc<pi_chord::sqlite::cloudflare::DurableObjectSqliteStorage>,
    options: SqliteStorageOptions,
) -> StorageFuture<SqliteStorage> {
    SqliteStorage::open(
        Arc::new(pi_chord::sqlite::cloudflare::DurableObjectSqliteDatabase::new(storage)),
        options,
    )
}
fn required_conversation(row: Option<SqliteRow>, id: ConversationId) -> Result<ConversationRecord> {
    row.as_ref()
        .map(record)
        .transpose()?
        .ok_or_else(|| Error::request(format!("Unknown conversation: {id}")))
}
async fn read_conversation(
    db: &dyn SqliteExecutor,
    schema: &SqliteSchema,
    id: ConversationId,
) -> Result<ConversationRecord> {
    required_conversation(
        db.get(
            &format!(
                "SELECT record FROM {} WHERE id = ?",
                schema.tables.conversations
            ),
            &[id.get().into()],
        )
        .await?,
        id,
    )
}
impl Storage for SqliteStorage {
    fn commit(&self, writes: &[StorageWrite], adopt: Adopt, _: &Context) -> StorageFuture<Seq> {
        let mut state = self.inner.state.lock();
        if let Err(error) = assert_open(&state) {
            return ready(Err(error));
        }
        let actions = match prepare_actions(writes) {
            Ok(actions) => actions,
            Err(error) => return ready(Err(error)),
        };
        let candidate = writes
            .iter()
            .filter_map(write_id)
            .map(|(_, id)| id + 1)
            .fold(state.next_id, i64::max);
        let (send, receive) = tokio::sync::oneshot::channel();
        let previous = std::mem::replace(
            &mut state.publication,
            async move {
                let _ = receive.await;
            }
            .boxed()
            .shared(),
        );
        let publication = Publication(Some(send));
        let writes = writes.to_vec();
        let inner = self.inner.clone();
        let schema = inner.schema.clone();
        let transaction = inner.database.transaction(move |tx| async move {
            previous.await;
            let result: Result<Seq> = async {
                let metadata = tx
                    .get(
                        &format!(
                            "SELECT next_id, next_seq FROM {} WHERE singleton = 1",
                            schema.tables.durable_metadata
                        ),
                        &[],
                    )
                    .await?
                    .ok_or_else(|| Error::message("Durable SQLite metadata is missing"))?;
                let seq = integer(&metadata, "next_seq")?;
                if seq.abs() > MAX_SAFE_INTEGER {
                    return Err(Error::message("expected a safe integer"));
                }
                let seq = Seq::from_raw(seq);
                check_ids(tx.as_ref(), &schema, &writes).await?;
                check_actions(tx.as_ref(), &schema, &actions).await?;
                for write in &writes {
                    apply_table_write(tx.as_ref(), &schema, write, seq).await?;
                }
                apply_actions(tx.as_ref(), &schema, actions, seq).await?;
                let stored_next = text(&metadata, "next_id")?
                    .parse::<i64>()
                    .map_err(|e| Error::message(e.to_string()))?;
                tx.run(
                    &format!(
                        "UPDATE {} SET next_id = ?, next_seq = ? WHERE singleton = 1",
                        schema.tables.durable_metadata
                    ),
                    &[
                        sql_text(stored_next.max(candidate).to_string()),
                        (seq.get() + 1).into(),
                    ],
                )
                .await?;
                Ok(seq)
            }
            .await;
            result.map_err(SqliteError::external)
        });
        eager(async move {
            let _publication = publication;
            let seq = transaction.await?;
            {
                let mut state = inner.state.lock();
                state.next_id = state.next_id.max(candidate);
            }
            adopt(seq);
            Ok(seq)
        })
    }
    fn mint_id(&self) -> StorageFuture<i64> {
        let mut state = self.inner.state.lock();
        let result = assert_open(&state).and_then(|_| {
            if state.next_id.abs() > MAX_SAFE_INTEGER {
                return Err(Error::message("ID space is exhausted"));
            }
            let id = state.next_id;
            state.next_id += 1;
            Ok(id)
        });
        ready(result)
    }
    fn conversation(
        &self,
        id: ConversationId,
        _: &Context,
    ) -> StorageFuture<Option<ConversationRecord>> {
        self.get_record(
            &self.inner.schema.tables.conversations,
            "id = ?",
            &[id.get().into()],
        )
    }
    fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<ConversationRecord>> {
        if let Err(error) = self.ensure_open() {
            return ready(Err(error));
        }
        let start = match scan::scan_start(query.order, cursor, ScanOrder::Ascending) {
            Ok(start) => start,
            Err(error) => return ready(Err(error)),
        };
        let mut sql = ScanSql::new(&start);
        if let Some(id) = query.owner_conversation_id {
            sql.and("owner_conversation_id = ?", id.get().into());
        }
        if let Some(id) = query.owner_task_id {
            sql.and("owner_task_id = ?", id.get().into());
        }
        self.scan(
            &self.inner.schema.tables.conversations,
            sql,
            limit,
            start.order,
            |record: &ConversationRecord| record.id.get(),
        )
    }
    fn entry(&self, id: EntryId, _: &Context) -> StorageFuture<Option<StoredEntry>> {
        self.entry_read(None, id)
    }
    fn entry_in(
        &self,
        conversation: ConversationId,
        id: EntryId,
        _: &Context,
    ) -> StorageFuture<Option<StoredEntry>> {
        self.entry_read(Some(conversation), id)
    }
    fn find_latest_head_marker(
        &self,
        conversation: ConversationId,
        at: Option<EntryId>,
        _: &Context,
    ) -> StorageFuture<Option<EntryRecord>> {
        let mut state = self.inner.state.lock();
        if let Err(error) = assert_open(&state) {
            return ready(Err(error));
        }
        state.admitted_reads += 1;
        let guard = ReadGuard(self.inner.clone());
        let inner = self.inner.clone();
        let first = inner.database.get(
            &format!(
                "SELECT record FROM {} WHERE id = ?",
                inner.schema.tables.conversations
            ),
            &[conversation.get().into()],
        );
        eager(async move {
            let _guard = guard;
            let mut conversation = required_conversation(first.await?, conversation)?;
            let mut upper = at.map(EntryId::get);
            loop {
                let mut params = vec![conversation.id.get().into()];
                let cutoff = if let Some(upper) = upper {
                    params.push(upper.into());
                    " AND id <= ?"
                } else {
                    ""
                };
                let row = inner.database.get(&format!("SELECT record FROM {} WHERE conversation_id = ? AND head IS NOT NULL{cutoff} ORDER BY id DESC LIMIT 1", inner.schema.tables.entries), &params).await?;
                if let Some(row) = row {
                    return record(&row).map(Some);
                }
                let Some(parent) = conversation.parent else {
                    return Ok(None);
                };
                upper = Some(upper.unwrap_or(MAX_SAFE_INTEGER).min(parent.at.get()));
                conversation = read_conversation(
                    inner.database.as_ref(),
                    &inner.schema,
                    parent.conversation_id,
                )
                .await?;
            }
        })
    }
    fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<EntryRecord>> {
        let mut state = self.inner.state.lock();
        if let Err(error) = assert_open(&state) {
            return ready(Err(error));
        }
        let start = match scan::scan_start(query.order, cursor, ScanOrder::Descending) {
            Ok(start) => start,
            Err(error) => return ready(Err(error)),
        };
        state.admitted_reads += 1;
        let guard = ReadGuard(self.inner.clone());
        let inner = self.inner.clone();
        let query = query.clone();
        let first = inner.database.get(
            &format!(
                "SELECT record FROM {} WHERE id = ?",
                inner.schema.tables.conversations
            ),
            &[query.conversation_id.get().into()],
        );
        eager(async move {
            let _guard = guard;
            let mut conv = required_conversation(first.await?, query.conversation_id)?;
            let mut items = Vec::<EntryRecord>::new();
            let min = query.min_entry_id.map(EntryId::get);
            let mut upper = query.max_entry_id.map(EntryId::get);
            if start.order == ScanOrder::Descending {
                if let Some(after) = start.after {
                    upper = Some(upper.unwrap_or(MAX_SAFE_INTEGER).min(after - 1));
                }
                loop {
                    let mut params = vec![conv.id.get().into()];
                    let lower = if let Some(min) = min {
                        params.push(min.into());
                        " AND id >= ?"
                    } else {
                        ""
                    };
                    let cutoff = if let Some(upper) = upper {
                        params.push(upper.into());
                        " AND id <= ?"
                    } else {
                        ""
                    };
                    params
                        .push((limit.saturating_add(1).saturating_sub(items.len()) as i64).into());
                    let rows = inner.database.all(&format!("SELECT record FROM {} WHERE conversation_id = ?{lower}{cutoff} ORDER BY id DESC LIMIT ?", inner.schema.tables.entries), &params).await?;
                    items.extend(rows.iter().map(record).collect::<Result<Vec<_>>>()?);
                    if items.len() > limit {
                        break;
                    }
                    let Some(parent) = conv.parent else {
                        break;
                    };
                    upper = Some(upper.unwrap_or(MAX_SAFE_INTEGER).min(parent.at.get()));
                    if upper.zip(min).is_some_and(|(upper, min)| upper < min) {
                        break;
                    }
                    conv = read_conversation(
                        inner.database.as_ref(),
                        &inner.schema,
                        parent.conversation_id,
                    )
                    .await?;
                }
            } else {
                let mut segments = Vec::new();
                loop {
                    segments.push((conv.id, upper));
                    let Some(parent) = conv.parent else {
                        break;
                    };
                    upper = Some(upper.unwrap_or(MAX_SAFE_INTEGER).min(parent.at.get()));
                    if upper.zip(min).is_some_and(|(upper, min)| upper < min) {
                        break;
                    }
                    conv = read_conversation(
                        inner.database.as_ref(),
                        &inner.schema,
                        parent.conversation_id,
                    )
                    .await?;
                }
                let lower = start
                    .after
                    .map_or(min, |after| Some(min.unwrap_or(after + 1).max(after + 1)));
                for (id, upper) in segments.into_iter().rev() {
                    let mut params = vec![id.get().into()];
                    let lower = if let Some(lower) = lower {
                        params.push(lower.into());
                        " AND id >= ?"
                    } else {
                        ""
                    };
                    let cutoff = if let Some(upper) = upper {
                        params.push(upper.into());
                        " AND id <= ?"
                    } else {
                        ""
                    };
                    params
                        .push((limit.saturating_add(1).saturating_sub(items.len()) as i64).into());
                    let rows = inner.database.all(&format!("SELECT record FROM {} WHERE conversation_id = ?{lower}{cutoff} ORDER BY id ASC LIMIT ?", inner.schema.tables.entries), &params).await?;
                    items.extend(rows.iter().map(record).collect::<Result<Vec<_>>>()?);
                    if items.len() > limit {
                        break;
                    }
                }
            }
            Ok(page(items, limit, start.order, |entry| entry.id.get()))
        })
    }
    fn task(&self, id: TaskId, _: &Context) -> StorageFuture<Option<TaskRecord>> {
        self.get_record(
            &self.inner.schema.tables.tasks,
            "id = ?",
            &[id.get().into()],
        )
    }
    fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<TaskRecord>> {
        if let Err(error) = self.ensure_open() {
            return ready(Err(error));
        }
        let start = match scan::scan_start(query.order, cursor, ScanOrder::Ascending) {
            Ok(start) => start,
            Err(error) => return ready(Err(error)),
        };
        let mut sql = ScanSql::new(&start);
        if let Some(id) = query.conversation_id {
            sql.and("conversation_id = ?", id.get().into());
        }
        if let Some(kind) = &query.kind {
            sql.and("kind = ?", indexed(kind));
        }
        if let Some(status) = &query.status {
            sql.and("status = ?", token(status));
        }
        if let Some(abort) = query.abort_requested {
            sql.and("abort_requested = ?", i64::from(abort).into());
        }
        if let Some(background) = query.background {
            sql.and("background = ?", i64::from(background).into());
        }
        self.scan(
            &self.inner.schema.tables.tasks,
            sql,
            limit,
            start.order,
            |record: &TaskRecord| record.id().get(),
        )
    }
    fn submission(&self, id: SubmissionId, _: &Context) -> StorageFuture<Option<SubmissionRecord>> {
        self.get_record(
            &self.inner.schema.tables.submissions,
            "id = ?",
            &[id.get().into()],
        )
    }
    fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<SubmissionRecord>> {
        if let Err(error) = self.ensure_open() {
            return ready(Err(error));
        }
        let start = match scan::scan_start(query.order, cursor, ScanOrder::Ascending) {
            Ok(start) => start,
            Err(error) => return ready(Err(error)),
        };
        let mut sql = ScanSql::new(&start);
        if let Some(id) = query.conversation_id {
            sql.and("conversation_id = ?", id.get().into());
        }
        if let Some(status) = &query.status {
            sql.and("status = ?", token(status));
        }
        self.scan(
            &self.inner.schema.tables.submissions,
            sql,
            limit,
            start.order,
            |record: &SubmissionRecord| record.id().get(),
        )
    }
    fn submission_by_request(
        &self,
        conversation: ConversationId,
        request: &JsonString,
        _: &Context,
    ) -> StorageFuture<Option<SubmissionRecord>> {
        self.get_record(
            &self.inner.schema.tables.submissions,
            "conversation_id = ? AND request_id = ? LIMIT 1",
            &[conversation.get().into(), indexed(request)],
        )
    }
    fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        _: &Context,
    ) -> StorageFuture<Option<DocumentRecord>> {
        let mut params = address_parts(address);
        let alive = match at {
            DocumentPoint::Current => "retired_at IS NULL",
            DocumentPoint::At(seq) => {
                params.extend([seq.get().into(), seq.get().into()]);
                "created_at <= ? AND (retired_at IS NULL OR retired_at > ?)"
            }
        };
        self.get_record(&self.inner.schema.tables.documents, &format!("kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ? AND {alive} ORDER BY created_at DESC LIMIT 1"), &params)
    }
    fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        _: &Context,
    ) -> StorageFuture<Option<StoredDocument>> {
        let state = self.inner.state.lock();
        if let Err(error) = assert_open(&state) {
            return ready(Err(error));
        }
        let schema = self.inner.schema.clone();
        let transaction = self.inner.database.transaction(move |tx| async move {
            materialize(tx.as_ref(), &schema, id, at)
                .await
                .map_err(SqliteError::external)
        });
        eager(async move { Ok(transaction.await?) })
    }
    fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _: &Context,
    ) -> StorageFuture<Page<DocumentRecord>> {
        if let Err(error) = self.ensure_open() {
            return ready(Err(error));
        }
        let after = match scan::cursor_id(cursor) {
            Ok(after) => after,
            Err(error) => return ready(Err(error)),
        };
        let mut sql = ScanSql::new(&ScanStart {
            order: ScanOrder::Ascending,
            after,
        });
        let (scope, owner) = scope_parts(&query.scope);
        sql.and("scope_kind = ?", scope.into());
        sql.and("owner_id = ?", owner.into());
        if let Some(kind) = &query.kind {
            sql.and("kind = ?", indexed(kind));
        }
        match query.at {
            DocumentPoint::Current => sql.conditions.push("retired_at IS NULL".into()),
            DocumentPoint::At(seq) => {
                sql.and("created_at <= ?", seq.get().into());
                sql.and("(retired_at IS NULL OR retired_at > ?)", seq.get().into());
            }
        }
        self.scan(
            &self.inner.schema.tables.documents,
            sql,
            limit,
            ScanOrder::Ascending,
            |record: &DocumentRecord| record.id().get(),
        )
    }
    fn close(&self, _: &Context) -> StorageFuture<()> {
        let mut state = self.inner.state.lock();
        if let Some(closing) = &state.closing {
            return Box::pin(closing.clone());
        }
        state.closed = true;
        let inner = self.inner.clone();
        let closing = eager(async move {
            loop {
                let notified = inner.drained.notified();
                if inner.state.lock().admitted_reads == 0 {
                    break;
                }
                notified.await;
            }
            inner.database.close().await?;
            Ok(())
        })
        .shared();
        state.closing = Some(closing.clone());
        Box::pin(closing)
    }
}
#[derive(Default)]
struct Action {
    create: Option<DocumentCreate>,
    copy: Option<DocumentCopySource>,
    content: Option<DocumentContent>,
    retire: bool,
}
type Actions = IndexMap<DocumentId, Action>;
fn write_id(write: &StorageWrite) -> Option<(&'static str, i64)> {
    Some(match write {
        StorageWrite::Conversation { value } => ("conversation", value.id.get()),
        StorageWrite::Entry { value } => ("entry", value.id.get()),
        StorageWrite::Task { value } => ("task", value.id().get()),
        StorageWrite::Submission { value } => ("submission", value.id().get()),
        StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => {
            ("document", record.id.get())
        }
        _ => return None,
    })
}
fn prepare_actions(writes: &[StorageWrite]) -> Result<Actions> {
    let mut actions = Actions::new();
    for write in writes {
        let (id, create, content, copy, retire) = match write {
            StorageWrite::DocumentCreate { record, content } => (
                record.id,
                Some(record),
                Some(DocumentContent::Base {
                    version: content.version,
                    value: content.value.clone(),
                }),
                None,
                false,
            ),
            StorageWrite::DocumentCopy { record, source } => {
                (record.id, Some(record), None, Some(source), false)
            }
            StorageWrite::DocumentChange { id, content } => {
                (*id, None, Some(content.clone()), None, false)
            }
            StorageWrite::DocumentRetire { id } => (*id, None, None, None, true),
            _ => continue,
        };
        let action = actions.entry(id).or_default();
        if retire {
            if action.retire {
                return Err(Error::message(format!(
                    "Document {id} is retired more than once"
                )));
            }
            action.retire = true;
        } else {
            if action.content.is_some()
                || action.copy.is_some()
                || (create.is_some() && action.create.is_some())
            {
                return Err(Error::message(format!(
                    "Document {id} has more than one content command"
                )));
            }
            if let Some(create) = create {
                action.create = Some(create.clone());
            }
            action.content = content;
            action.copy = copy.cloned();
        }
    }
    Ok(actions)
}
async fn check_ids(
    tx: &dyn SqliteExecutor,
    schema: &SqliteSchema,
    writes: &[StorageWrite],
) -> Result<()> {
    let mut claimed = HashMap::new();
    for write in writes {
        let Some((table, id)) = write_id(write) else {
            continue;
        };
        let row = tx
            .get(
                &format!(
                    "SELECT record_type FROM {} WHERE id = ?",
                    schema.tables.record_ids
                ),
                &[id.into()],
            )
            .await?;
        let existing = row
            .as_ref()
            .map(|row| text(row, "record_type"))
            .transpose()?;
        let earlier = claimed.get(&id).copied();
        if matches!(table, "conversation" | "entry" | "document") {
            if let Some(existing) = existing {
                return Err(Error::message(format!(
                    "ID {id} already belongs to {existing}"
                )));
            }
            if earlier.is_some() {
                return Err(Error::message(format!("ID {id} is written more than once")));
            }
        } else {
            if let Some(existing) = existing
                && existing != table
            {
                return Err(Error::message(format!(
                    "ID {id} already belongs to {existing}"
                )));
            }
            if earlier.is_some_and(|earlier| earlier != table) {
                return Err(Error::message(format!(
                    "ID {id} is written as two record types"
                )));
            }
        }
        claimed.insert(id, table);
    }
    Ok(())
}
async fn check_actions(
    tx: &dyn SqliteExecutor,
    schema: &SqliteSchema,
    actions: &Actions,
) -> Result<()> {
    let mut live_counts = HashMap::<DocumentAddress, i64>::new();
    for (id, action) in actions {
        if let Some(copy) = &action.copy
            && actions.contains_key(&copy.id)
        {
            return Err(Error::message(format!(
                "Document copy {id} source is changed in the copy batch"
            )));
        }
        let row = tx
            .get(
                &format!(
                    "SELECT record FROM {} WHERE id = ?",
                    schema.tables.documents
                ),
                &[id.get().into()],
            )
            .await?;
        let existing = row.as_ref().map(record::<DocumentRecord>).transpose()?;
        if action.create.is_none() && existing.is_none() {
            return Err(Error::message(format!("Unknown document: {id}")));
        }
        if action.create.is_some() && existing.is_some() {
            return Err(Error::message(format!("Document {id} already exists")));
        }
        if existing
            .as_ref()
            .is_some_and(|record| record.retired_at.is_some())
        {
            return Err(Error::message(format!("Document {id} is retired")));
        }
        if let Some(DocumentContent::Delta { version, .. }) = &action.content {
            let previous = tx
                .get(
                    &format!(
                        "SELECT version FROM {} WHERE document_id = ? ORDER BY seq DESC LIMIT 1",
                        schema.tables.document_revisions
                    ),
                    &[id.get().into()],
                )
                .await?
                .ok_or_else(|| Error::message(format!("Document {id} delta has no base")))?;
            if integer(&previous, "version")? as u64 != *version {
                return Err(Error::message(format!(
                    "Document {id} version transition requires a base"
                )));
            }
        }
        let address = action
            .create
            .as_ref()
            .unwrap_or_else(|| &existing.as_ref().unwrap().create)
            .address();
        let mut live = match live_counts.get(&address) {
            Some(live) => *live,
            None => i64::from(tx.get(&format!("SELECT id FROM {} WHERE kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ? AND retired_at IS NULL LIMIT 1", schema.tables.documents), &address_parts(&address)).await?.is_some()),
        };
        if action.retire && existing.is_some() {
            live -= 1;
        }
        if action.create.is_some() && !action.retire {
            live += 1;
        }
        live_counts.insert(address, live);
    }
    if live_counts.values().any(|live| *live > 1) {
        return Err(Error::message(
            "Document address already has a current incarnation",
        ));
    }
    Ok(())
}
async fn claim_id(
    tx: &dyn SqliteExecutor,
    schema: &SqliteSchema,
    id: i64,
    table: &str,
) -> Result<()> {
    tx.run(
        &format!(
            "INSERT OR IGNORE INTO {} (id, record_type) VALUES (?, ?)",
            schema.tables.record_ids
        ),
        &[id.into(), table.into()],
    )
    .await?;
    Ok(())
}
async fn apply_table_write(
    tx: &dyn SqliteExecutor,
    schema: &SqliteSchema,
    write: &StorageWrite,
    seq: Seq,
) -> Result<()> {
    let t = &schema.tables;
    match write {
        StorageWrite::Conversation { value } => {
            claim_id(tx, schema, value.id.get(), "conversation").await?;
            tx.run(&format!("INSERT INTO {} (id, owner_conversation_id, owner_task_id, record) VALUES (?, ?, ?, ?)", t.conversations), &[
                value.id.get().into(), opt_id(value.owner.as_ref().map(|owner| owner.conversation_id.get())), opt_id(value.owner.as_ref().map(|owner| owner.task_id.get())), sql_text(encode(value)?),
            ]).await?;
        }
        StorageWrite::Entry { value } => {
            claim_id(tx, schema, value.id.get(), "entry").await?;
            tx.run(&format!("INSERT INTO {} (id, conversation_id, head, commit_seq, record) VALUES (?, ?, ?, ?, ?)", t.entries), &[
                value.id.get().into(), value.conversation_id.get().into(), opt_id(value.head.map(EntryId::get)), seq.get().into(), sql_text(encode(value)?),
            ]).await?;
        }
        StorageWrite::Task { value } => {
            claim_id(tx, schema, value.id().get(), "task").await?;
            tx.run(&format!("INSERT INTO {} (id, conversation_id, kind, status, abort_requested, background, record) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET conversation_id = excluded.conversation_id, kind = excluded.kind, status = excluded.status, abort_requested = excluded.abort_requested, background = excluded.background, record = excluded.record", t.tasks), &[
                value.id().get().into(), value.info.conversation_id.get().into(), indexed(&value.info.kind), token(&value.state.status()), i64::from(value.info.abort_requested).into(), i64::from(value.info.background).into(), sql_text(encode(value)?),
            ]).await?;
        }
        StorageWrite::Submission { value } => {
            claim_id(tx, schema, value.id().get(), "submission").await?;
            tx.run(&format!("INSERT INTO {} (id, conversation_id, request_id, status, record) VALUES (?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET conversation_id = excluded.conversation_id, request_id = excluded.request_id, status = excluded.status, record = excluded.record", t.submissions), &[
                value.id().get().into(), value.info.conversation_id.get().into(), value.info.request_id.as_ref().map_or(SqliteValue::Null, indexed), token(&value.state.status()), sql_text(encode(value)?),
            ]).await?;
        }
        _ => {}
    }
    Ok(())
}
async fn materialize(
    tx: &dyn SqliteExecutor,
    schema: &SqliteSchema,
    id: DocumentId,
    at: DocumentPoint,
) -> Result<Option<StoredDocument>> {
    let Some(row) = tx
        .get(
            &format!(
                "SELECT record FROM {} WHERE id = ?",
                schema.tables.documents
            ),
            &[id.get().into()],
        )
        .await?
    else {
        return Ok(None);
    };
    let record: DocumentRecord = record(&row)?;
    if at != DocumentPoint::Current && record.create.semantics.current_only() {
        return Err(Error::request(format!(
            "Document {id} does not retain historical content"
        )));
    }
    if !record.alive_at(at) {
        return Ok(None);
    }
    let upper = match at {
        DocumentPoint::Current => MAX_SAFE_INTEGER,
        DocumentPoint::At(seq) => seq.get(),
    };
    let base = tx.get(&format!("SELECT seq, kind, version, content FROM {} WHERE document_id = ? AND kind = 'base' AND seq <= ? ORDER BY seq DESC LIMIT 1", schema.tables.document_revisions), &[id.get().into(), upper.into()]).await?
        .ok_or_else(|| Error::message(format!("Document {id} is missing a required base")))?;
    let version = integer(&base, "version")? as u64;
    let value: JsonObject = serde_json::from_str(&text(&base, "content")?)?;
    let tail = tx.all(&format!("SELECT seq, kind, version, content FROM {} WHERE document_id = ? AND seq > ? AND seq <= ? ORDER BY seq", schema.tables.document_revisions), &[id.get().into(), integer(&base, "seq")?.into(), upper.into()]).await?;
    let mut batches = Vec::<Vec<Op>>::with_capacity(tail.len());
    for row in &tail {
        if text(row, "kind")? != "delta" || integer(row, "version")? as u64 != version {
            return Err(Error::message(format!(
                "Document {id} crosses a stored version boundary without a base"
            )));
        }
        batches.push(serde_json::from_str(&text(row, "content")?)?);
    }
    let value = apply_immutable_batches(Some(value.value()), batches.iter().map(Vec::as_slice))?;
    Ok(Some(StoredDocument {
        record,
        version,
        value: JsonObject::try_from_value(value).map_err(Error::message)?,
        deltas_since_base: tail.len(),
    }))
}
async fn apply_actions(
    tx: &dyn SqliteExecutor,
    schema: &SqliteSchema,
    actions: Actions,
    seq: Seq,
) -> Result<()> {
    for (id, action) in actions {
        let mut content = action.content;
        if let Some(copy) = action.copy {
            let copied: Result<DocumentContent> = async {
                let stored = materialize(tx, schema, copy.id, copy.at)
                    .await?
                    .ok_or_else(|| {
                        Error::message(format!("Fork source document {} cannot be read", copy.id))
                    })?;
                let create = action.create.as_ref().unwrap();
                let compatible = match (&stored.record.create.semantics, &create.semantics) {
                    (
                        DocumentSemantics::Conversation { history: a, .. },
                        DocumentSemantics::Conversation { history: b, .. },
                    ) => a == b,
                    _ => false,
                };
                if !compatible
                    || stored.record.create.kind != create.kind
                    || stored.record.create.key != create.key
                {
                    return Err(Error::message(format!(
                        "Fork source document {} does not match the copied record",
                        copy.id
                    )));
                }
                Ok(DocumentContent::Base {
                    version: stored.version,
                    value: stored.value,
                })
            }
            .await;
            content = Some(copied.map_err(|cause| Error::document_copy(id, cause))?);
        }
        let created = action.create.is_some();
        let mut record = if let Some(create) = action.create {
            let record = DocumentRecord {
                create,
                created_at: seq,
                retired_at: action.retire.then_some(seq),
            };
            let mut params = vec![id.get().into()];
            let parts = address_parts(&record.create.address());
            // address order is kind, scope, owner, family, key; row order groups family/key.
            params.extend([
                parts[0].clone(),
                parts[3].clone(),
                parts[4].clone(),
                parts[1].clone(),
                parts[2].clone(),
                seq.get().into(),
                opt_id(record.retired_at.map(Seq::get)),
                sql_text(encode(&record)?),
            ]);
            claim_id(tx, schema, id.get(), "document").await?;
            tx.run(&format!("INSERT INTO {} (id, kind, family, key_value, scope_kind, owner_id, created_at, retired_at, record) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)", schema.tables.documents), &params).await?;
            record
        } else {
            let row = tx
                .get(
                    &format!(
                        "SELECT record FROM {} WHERE id = ?",
                        schema.tables.documents
                    ),
                    &[id.get().into()],
                )
                .await?
                .expect("validated document");
            record::<DocumentRecord>(&row)?
        };
        if let Some(content) = content {
            if matches!(content, DocumentContent::Base { .. })
                && record.create.semantics.current_only()
            {
                tx.run(
                    &format!(
                        "DELETE FROM {} WHERE document_id = ?",
                        schema.tables.document_revisions
                    ),
                    &[id.get().into()],
                )
                .await?;
            }
            let (kind, encoded) = match &content {
                DocumentContent::Base { value, .. } => ("base", encode(value)?),
                DocumentContent::Delta { ops, .. } => ("delta", encode(ops)?),
            };
            tx.run(&format!("INSERT INTO {} (document_id, seq, kind, version, content) VALUES (?, ?, ?, ?, ?)", schema.tables.document_revisions), &[
                id.get().into(), seq.get().into(), kind.into(), (content.version() as i64).into(), sql_text(encoded),
            ]).await?;
        }
        if action.retire {
            if !created {
                record.retired_at = Some(seq);
                tx.run(
                    &format!(
                        "UPDATE {} SET retired_at = ?, record = ? WHERE id = ?",
                        schema.tables.documents
                    ),
                    &[
                        seq.get().into(),
                        sql_text(encode(&record)?),
                        id.get().into(),
                    ],
                )
                .await?;
            }
            if record.create.semantics.current_only() {
                tx.run(
                    &format!(
                        "DELETE FROM {} WHERE document_id = ?",
                        schema.tables.document_revisions
                    ),
                    &[id.get().into()],
                )
                .await?;
            }
        }
    }
    Ok(())
}
