//! The surface every hook and task phase gets from its runtime, as upstream's `HookApi` and
//! `DocumentReader`: the task and conversation it runs for, the current models, memos, and
//! document snapshots. Runtime objects of every task kind implement it, so hooks can be written
//! once over the trait.

use super::runtime::TaskRuntime;
use crate::{TaskValue, storage::StorageFuture, *};
use pi_ai::models::Models;
use std::sync::Arc;

/// Read access to documents: `snapshot` and `snapshotAsOf` of upstream's `DocumentReader`.
pub trait DocumentReader {
    /// The current value of a document, or `None` when it does not exist.
    fn snapshot<T: DocumentState, D: DocScope>(
        &self,
        token: &DocToken<T, D>,
        scope: D::Owner,
        context: &Context,
    ) -> StorageFuture<Option<Arc<T>>>;
    /// A rewindable document as of an entry.
    fn snapshot_as_of<T: DocumentState, D: RewindableScope>(
        &self,
        token: &DocToken<T, D>,
        owner: D::Owner,
        at: EntryId,
        context: &Context,
    ) -> StorageFuture<Option<Arc<T>>>;
}

/// What hooks and phases of the built-in tasks receive.
pub trait HookApi: DocumentReader {
    fn task_id(&self) -> TaskId;
    fn conversation_id(&self) -> ConversationId;
    /// The current `HarnessOptions.models`, read at each access.
    fn models(&self) -> Result<Models>;
    /// A memo stored earlier by this task, `None` when there is none.
    fn memo<T: TaskValue>(&self, name: &str, context: &Context) -> StorageFuture<Option<T>>;
    /// Store `candidate` unless a memo of that name exists; the memo that stands, as upstream's
    /// two-argument-plus-candidate `memo`.
    fn store_memo<T: TaskValue>(
        &self,
        name: &str,
        candidate: T,
        context: &Context,
    ) -> StorageFuture<T>;
}

impl<I, S, R, H> DocumentReader for TaskRuntime<I, S, R, H> {
    fn snapshot<T: DocumentState, D: DocScope>(
        &self,
        token: &DocToken<T, D>,
        scope: D::Owner,
        context: &Context,
    ) -> StorageFuture<Option<Arc<T>>> {
        TaskRuntime::snapshot(self, token, scope, context)
    }
    fn snapshot_as_of<T: DocumentState, D: RewindableScope>(
        &self,
        token: &DocToken<T, D>,
        owner: D::Owner,
        at: EntryId,
        context: &Context,
    ) -> StorageFuture<Option<Arc<T>>> {
        TaskRuntime::snapshot_as_of(self, token, owner, at, context)
    }
}
impl<I, S, R, H> HookApi for TaskRuntime<I, S, R, H> {
    fn task_id(&self) -> TaskId {
        TaskRuntime::task_id(self).untyped()
    }
    fn conversation_id(&self) -> ConversationId {
        TaskRuntime::conversation_id(self)
    }
    fn models(&self) -> Result<Models> {
        TaskRuntime::models(self)
    }
    fn memo<T: TaskValue>(&self, name: &str, context: &Context) -> StorageFuture<Option<T>> {
        TaskRuntime::memo(self, name, context)
    }
    fn store_memo<T: TaskValue>(
        &self,
        name: &str,
        candidate: T,
        context: &Context,
    ) -> StorageFuture<T> {
        TaskRuntime::store_memo(self, name, candidate, context)
    }
}
