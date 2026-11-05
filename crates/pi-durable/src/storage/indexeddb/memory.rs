//! A host-side IndexedDB with the semantics the durable store relies on:
//! schema-defined stores and compound indexes, IndexedDB key ordering,
//! all-or-nothing readwrite transactions, and transactions that take effect in
//! creation order. It plays the role `fake-indexeddb` plays for the reference tests.
use super::{
    backend::{Accept, IdbBackend, IdbFuture, IdbMode, IdbTransaction, Source},
    key::{Direction, IdbKey, IdbKeyRange},
    schema::{self, StoreSpec},
};
use crate::{Error, JsonValue, Result};
use futures::{
    FutureExt,
    channel::oneshot,
    future::{BoxFuture, Shared},
};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

type Table = Arc<BTreeMap<IdbKey, JsonValue>>;
type Tables = HashMap<&'static str, Table>;
type Turn = Shared<BoxFuture<'static, ()>>;

struct Database {
    committed: Mutex<Tables>,
    /// Settles once every transaction created so far no longer blocks its successors.
    tail: Mutex<Turn>,
}

/// Named in-memory databases, like one origin's `indexedDB` factory.
#[derive(Clone, Default)]
pub struct MemoryIdbFactory {
    databases: Arc<Mutex<HashMap<String, Arc<Database>>>>,
}
impl MemoryIdbFactory {
    pub fn new() -> Self {
        Self::default()
    }
    /// Open a connection, creating the durable schema when the database is new.
    pub fn open(&self, name: &str) -> MemoryIdbConnection {
        let database = self
            .databases
            .lock()
            .entry(name.into())
            .or_insert_with(|| {
                let mut tables: Tables = schema::SCHEMA
                    .iter()
                    .map(|store| (store.name, Table::default()))
                    .collect();
                let meta = schema::initial_meta();
                let key = schema::store(schema::META)
                    .and_then(|store| store.key_path.extract(&meta))
                    .expect("meta key");
                Arc::make_mut(tables.get_mut(schema::META).unwrap()).insert(key, meta);
                Arc::new(Database {
                    committed: Mutex::new(tables),
                    tail: Mutex::new(async {}.boxed().shared()),
                })
            })
            .clone();
        MemoryIdbConnection {
            database,
            closed: AtomicBool::new(false),
        }
    }
    /// Open a connection to an existing database without creating its schema.
    /// The connection sees no stores when the database is unknown.
    pub fn open_existing(&self, name: &str) -> Option<MemoryIdbConnection> {
        let database = self.databases.lock().get(name)?.clone();
        Some(MemoryIdbConnection {
            database,
            closed: AtomicBool::new(false),
        })
    }
    pub fn delete(&self, name: &str) {
        self.databases.lock().remove(name);
    }
    /// Every committed row of one object store in key order.
    pub fn rows(&self, name: &str, store: &str) -> Vec<JsonValue> {
        self.databases
            .lock()
            .get(name)
            .map_or_else(Vec::new, |database| {
                database
                    .committed
                    .lock()
                    .get(store)
                    .map_or_else(Vec::new, |table| table.values().cloned().collect())
            })
    }
    /// Replace one object store's committed rows, keyed by the schema key path,
    /// e.g. to load rows written by the reference implementation.
    pub fn load(&self, name: &str, store: &str, rows: Vec<JsonValue>) -> Result<()> {
        let spec = spec(store)?;
        let connection = self.open(name);
        let mut table = BTreeMap::new();
        for row in rows {
            table.insert(primary_key(spec, &row)?, row);
        }
        connection
            .database
            .committed
            .lock()
            .insert(spec.name, Arc::new(table));
        Ok(())
    }
}

pub struct MemoryIdbConnection {
    database: Arc<Database>,
    closed: AtomicBool,
}
impl IdbBackend for MemoryIdbConnection {
    type Transaction = MemoryIdbTransaction;
    fn transaction(&self, stores: &[&'static str], mode: IdbMode) -> Result<Self::Transaction> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::message(
                "InvalidStateError: The database connection is closing.",
            ));
        }
        for store in stores {
            spec(store)?;
        }
        let (release, released) = oneshot::channel::<()>();
        let previous = {
            let mut tail = self.database.tail.lock();
            let previous = tail.clone();
            let after = previous.clone();
            *tail = async move {
                after.await;
                let _ = released.await;
            }
            .boxed()
            .shared();
            previous
        };
        Ok(MemoryIdbTransaction {
            database: self.database.clone(),
            scope: stores.to_vec(),
            mode,
            previous,
            release: Mutex::new(Some(release)),
            tables: Mutex::new(None),
        })
    }
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

pub struct MemoryIdbTransaction {
    database: Arc<Database>,
    scope: Vec<&'static str>,
    mode: IdbMode,
    previous: Turn,
    release: Mutex<Option<oneshot::Sender<()>>>,
    /// The snapshot (readonly) or working copy (readwrite), taken at start.
    tables: Mutex<Option<Tables>>,
}
fn spec(store: &str) -> Result<&'static StoreSpec> {
    schema::store(store).ok_or_else(|| {
        Error::message(format!(
            "NotFoundError: No objectStore named {store} in this database"
        ))
    })
}
fn primary_key(spec: &StoreSpec, row: &JsonValue) -> Result<IdbKey> {
    spec.key_path.extract(row).ok_or_else(|| {
        Error::message("DataError: Evaluating the object store's key path did not yield a value.")
    })
}
impl MemoryIdbTransaction {
    /// Wait for every earlier transaction, then take this transaction's view.
    async fn start(&self) -> Result<()> {
        self.previous.clone().await;
        let mut tables = self.tables.lock();
        if tables.is_none() {
            *tables = Some(self.database.committed.lock().clone());
            if self.mode == IdbMode::ReadOnly {
                // Later writers cannot change this snapshot.
                self.release.lock().take();
            }
        }
        Ok(())
    }
    fn table(&self, store: &str) -> Result<&'static StoreSpec> {
        if !self.scope.contains(&store) {
            return Err(Error::message(format!(
                "NotFoundError: {store} is not in this transaction's scope"
            )));
        }
        spec(store)
    }
    async fn read<T>(
        &self,
        store: &str,
        read: impl FnOnce(&BTreeMap<IdbKey, JsonValue>) -> T,
    ) -> Result<T> {
        self.table(store)?;
        self.start().await?;
        let tables = self.tables.lock();
        Ok(read(&tables.as_ref().expect("started")[store]))
    }
    async fn write<T>(
        &self,
        store: &str,
        write: impl FnOnce(&'static StoreSpec, &mut BTreeMap<IdbKey, JsonValue>) -> Result<T>,
    ) -> Result<T> {
        let spec = self.table(store)?;
        if self.mode == IdbMode::ReadOnly {
            return Err(Error::message(
                "ReadOnlyError: The transaction is read-only.",
            ));
        }
        self.start().await?;
        let mut tables = self.tables.lock();
        let table = tables
            .as_mut()
            .expect("started")
            .get_mut(store)
            .expect("schema store");
        write(spec, Arc::make_mut(table))
    }
    /// `(index key, primary key, row)` for each row the index covers, in index order.
    fn index_rows(
        spec: &StoreSpec,
        index: &str,
        table: &BTreeMap<IdbKey, JsonValue>,
    ) -> Result<Vec<(IdbKey, JsonValue)>> {
        let index = spec
            .indexes
            .iter()
            .find(|candidate| candidate.name == index)
            .ok_or_else(|| Error::message(format!("NotFoundError: No index named {index}")))?;
        let mut rows: Vec<_> = table
            .iter()
            .filter_map(|(primary, row)| {
                index
                    .key_path
                    .extract(row)
                    .map(|key| (key, primary.clone(), row.clone()))
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        Ok(rows.into_iter().map(|(key, _, row)| (key, row)).collect())
    }
}
impl IdbTransaction for MemoryIdbTransaction {
    fn get(&self, store: &'static str, key: IdbKey) -> IdbFuture<'_, Option<JsonValue>> {
        Box::pin(async move { self.read(store, |table| table.get(&key).cloned()).await })
    }
    fn index_get(
        &self,
        store: &'static str,
        index: &'static str,
        key: IdbKey,
    ) -> IdbFuture<'_, Option<JsonValue>> {
        Box::pin(async move {
            let spec = self.table(store)?;
            self.read(store, |table| Self::index_rows(spec, index, table))
                .await??
                .into_iter()
                .find(|(candidate, _)| *candidate == key)
                .map(|(_, row)| Ok(row))
                .transpose()
        })
    }
    fn add(&self, store: &'static str, row: JsonValue) -> IdbFuture<'_, ()> {
        Box::pin(async move {
            self.write(store, |spec, table| {
                let key = primary_key(spec, &row)?;
                if table.contains_key(&key) {
                    return Err(Error::message(
                        "ConstraintError: A record with the given key already exists",
                    ));
                }
                table.insert(key, row);
                Ok(())
            })
            .await
        })
    }
    fn put(&self, store: &'static str, row: JsonValue) -> IdbFuture<'_, ()> {
        Box::pin(async move {
            self.write(store, |spec, table| {
                table.insert(primary_key(spec, &row)?, row);
                Ok(())
            })
            .await
        })
    }
    fn delete(&self, store: &'static str, range: IdbKeyRange) -> IdbFuture<'_, ()> {
        Box::pin(async move {
            self.write(store, |_, table| {
                table.retain(|key, _| !range.contains(key));
                Ok(())
            })
            .await
        })
    }
    fn values(
        &self,
        source: Source,
        range: Option<IdbKeyRange>,
        direction: Direction,
        limit: usize,
        accept: Option<Accept>,
    ) -> IdbFuture<'_, Vec<JsonValue>> {
        Box::pin(async move {
            let spec = self.table(source.store)?;
            let mut rows = self
                .read(source.store, |table| match source.index {
                    Some(index) => Self::index_rows(spec, index, table),
                    None => Ok(table
                        .iter()
                        .map(|(key, row)| (key.clone(), row.clone()))
                        .collect()),
                })
                .await??;
            if direction == Direction::Prev {
                rows.reverse();
            }
            Ok(rows
                .into_iter()
                .filter(|(key, _)| range.as_ref().is_none_or(|range| range.contains(key)))
                .map(|(_, row)| row)
                .filter(|row| accept.as_ref().is_none_or(|accept| accept(row)))
                .take(limit)
                .collect())
        })
    }
    fn finish(self) -> IdbFuture<'static, ()> {
        Box::pin(async move {
            if self.mode == IdbMode::ReadWrite {
                self.start().await?;
                let tables = self.tables.lock().take().expect("started");
                *self.database.committed.lock() = tables;
            }
            Ok(())
        })
    }
    fn abort(self) -> IdbFuture<'static, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::indexeddb::schema::{CONVERSATIONS, CONVERSATIONS_BY_OWNER_TASK};

    fn row(id: i64, task: Option<i64>) -> JsonValue {
        let text = match task {
            Some(task) => format!(r#"{{"id":{id},"ownerTaskId":{task},"record":{{"id":{id}}}}}"#),
            None => format!(r#"{{"id":{id},"record":{{"id":{id}}}}}"#),
        };
        pi_chord::json::parse(&text).unwrap()
    }
    async fn ids(tx: &MemoryIdbTransaction, source: Source, direction: Direction) -> Vec<i64> {
        tx.values(source, None, direction, usize::MAX, None)
            .await
            .unwrap()
            .iter()
            .map(|row| crate::wire::get(row, "id").unwrap().as_f64().unwrap() as i64)
            .collect()
    }

    #[tokio::test]
    async fn transactions_take_effect_in_creation_order() {
        let connection = MemoryIdbFactory::new().open("order");
        let before = connection
            .transaction(&[CONVERSATIONS], IdbMode::ReadOnly)
            .unwrap();
        let write = connection
            .transaction(&[CONVERSATIONS], IdbMode::ReadWrite)
            .unwrap();
        let after = connection
            .transaction(&[CONVERSATIONS], IdbMode::ReadOnly)
            .unwrap();
        assert_eq!(before.get(CONVERSATIONS, 1.into()).await.unwrap(), None);
        write.add(CONVERSATIONS, row(1, None)).await.unwrap();
        // The later reader cannot start until the writer commits.
        let later = tokio::spawn(async move {
            let found = after.get(CONVERSATIONS, 1.into()).await.unwrap();
            after.finish().await.unwrap();
            found
        });
        tokio::task::yield_now().await;
        assert!(!later.is_finished());
        // The earlier reader keeps the view it started with.
        assert_eq!(before.get(CONVERSATIONS, 1.into()).await.unwrap(), None);
        before.finish().await.unwrap();
        write.finish().await.unwrap();
        assert_eq!(later.await.unwrap(), Some(row(1, None)));
    }

    #[tokio::test]
    async fn aborted_and_dropped_writes_leave_no_rows() {
        let factory = MemoryIdbFactory::new();
        let connection = factory.open("abort");
        let tx = connection
            .transaction(&[CONVERSATIONS], IdbMode::ReadWrite)
            .unwrap();
        tx.add(CONVERSATIONS, row(1, None)).await.unwrap();
        let error = tx.add(CONVERSATIONS, row(1, None)).await.unwrap_err();
        assert!(error.to_string().starts_with("ConstraintError"), "{error}");
        tx.abort().await.unwrap();
        let dropped = connection
            .transaction(&[CONVERSATIONS], IdbMode::ReadWrite)
            .unwrap();
        dropped.put(CONVERSATIONS, row(2, None)).await.unwrap();
        drop(dropped);
        assert!(factory.rows("abort", CONVERSATIONS).is_empty());
        let read = connection
            .transaction(&[CONVERSATIONS], IdbMode::ReadOnly)
            .unwrap();
        let error = read.put(CONVERSATIONS, row(3, None)).await.unwrap_err();
        assert!(error.to_string().starts_with("ReadOnlyError"), "{error}");
    }

    #[tokio::test]
    async fn indexes_skip_rows_without_their_key_and_walk_both_directions() {
        let connection = MemoryIdbFactory::new().open("index");
        let tx = connection
            .transaction(&[CONVERSATIONS], IdbMode::ReadWrite)
            .unwrap();
        for (id, task) in [(4, Some(9)), (2, Some(9)), (3, None), (5, Some(1))] {
            tx.put(CONVERSATIONS, row(id, task)).await.unwrap();
        }
        let index = Source::index(CONVERSATIONS, CONVERSATIONS_BY_OWNER_TASK);
        assert_eq!(ids(&tx, index, Direction::Next).await, [5, 2, 4]);
        assert_eq!(ids(&tx, index, Direction::Prev).await, [4, 2, 5]);
        let store = Source::store(CONVERSATIONS);
        assert_eq!(ids(&tx, store, Direction::Next).await, [2, 3, 4, 5]);
        let range = IdbKeyRange::bound(
            IdbKey::from(vec![9.into(), 2.into()]),
            IdbKey::from(vec![9.into(), crate::MAX_SAFE_INTEGER.into()]),
            true,
            false,
        );
        let rows = tx
            .values(index, Some(range), Direction::Next, 10, None)
            .await
            .unwrap();
        assert_eq!(rows, [row(4, Some(9))]);
        let first = tx
            .index_get(
                CONVERSATIONS,
                CONVERSATIONS_BY_OWNER_TASK,
                vec![9.into(), 4.into()].into(),
            )
            .await
            .unwrap();
        assert_eq!(first, Some(row(4, Some(9))));
        tx.finish().await.unwrap();
    }
}
