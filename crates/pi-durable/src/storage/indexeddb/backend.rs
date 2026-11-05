//! The slice of IndexedDB the durable store uses. `IndexedDbStorage` is written
//! against these traits only, so its transaction logic runs unchanged over the
//! host in-memory database and the browser binding.
use super::key::{Direction, IdbKey, IdbKeyRange};
use crate::{JsonValue, Result};

#[cfg(not(target_arch = "wasm32"))]
mod bounds {
    use crate::{JsonValue, Result};
    pub trait MaybeSend: Send {}
    impl<T: Send + ?Sized> MaybeSend for T {}
    pub trait MaybeSync: Sync {}
    impl<T: Sync + ?Sized> MaybeSync for T {}
    pub type IdbFuture<'a, T> = futures::future::BoxFuture<'a, Result<T>>;
    pub type Accept = Box<dyn Fn(&JsonValue) -> bool + Send + Sync>;
}
// Browser IndexedDB handles are single-threaded JavaScript objects.
#[cfg(target_arch = "wasm32")]
mod bounds {
    use crate::{JsonValue, Result};
    pub trait MaybeSend {}
    impl<T: ?Sized> MaybeSend for T {}
    pub trait MaybeSync {}
    impl<T: ?Sized> MaybeSync for T {}
    pub type IdbFuture<'a, T> = futures::future::LocalBoxFuture<'a, Result<T>>;
    pub type Accept = Box<dyn Fn(&JsonValue) -> bool>;
}
pub use bounds::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdbMode {
    ReadOnly,
    ReadWrite,
}

/// An object store, or one of its indexes, to read through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    pub store: &'static str,
    pub index: Option<&'static str>,
}
impl Source {
    pub const fn store(store: &'static str) -> Self {
        Self { store, index: None }
    }
    pub const fn index(store: &'static str, index: &'static str) -> Self {
        Self {
            store,
            index: Some(index),
        }
    }
}

/// One open database connection.
pub trait IdbBackend: MaybeSend + MaybeSync + 'static {
    type Transaction: IdbTransaction;
    /// Create a transaction now. Transactions with overlapping scopes take effect
    /// in creation order, as `IDBDatabase.transaction()` guarantees.
    fn transaction(&self, stores: &[&'static str], mode: IdbMode) -> Result<Self::Transaction>;
    fn close(&self);
}

/// One IndexedDB transaction. Rows are the structured-clone values stored by the
/// reference implementation, represented as JSON.
pub trait IdbTransaction: MaybeSend + MaybeSync + 'static {
    /// `IDBObjectStore.get(key)`.
    fn get(&self, store: &'static str, key: IdbKey) -> IdbFuture<'_, Option<JsonValue>>;
    /// `IDBIndex.get(key)`: the first matching row in index order.
    fn index_get(
        &self,
        store: &'static str,
        index: &'static str,
        key: IdbKey,
    ) -> IdbFuture<'_, Option<JsonValue>>;
    /// `IDBObjectStore.add(row)`; fails when the primary key already exists.
    fn add(&self, store: &'static str, row: JsonValue) -> IdbFuture<'_, ()>;
    /// `IDBObjectStore.put(row)`.
    fn put(&self, store: &'static str, row: JsonValue) -> IdbFuture<'_, ()>;
    /// `IDBObjectStore.delete(range)`.
    fn delete(&self, store: &'static str, range: IdbKeyRange) -> IdbFuture<'_, ()>;
    /// Walk a cursor, collecting rows that `accept` admits until `limit` rows are
    /// collected or the range is exhausted.
    fn values(
        &self,
        source: Source,
        range: Option<IdbKeyRange>,
        direction: Direction,
        limit: usize,
        accept: Option<Accept>,
    ) -> IdbFuture<'_, Vec<JsonValue>>;
    /// Wait for the transaction to commit.
    fn finish(self) -> IdbFuture<'static, ()>;
    /// Abort the transaction (if still possible) and wait for it to settle.
    fn abort(self) -> IdbFuture<'static, ()>;
}
