//! Commit-ordered agent events backed by the shared conversation mount.
mod translation;
mod types;
pub use translation::{EventBatch, EventTranslator, message_changes, snapshot_of};
pub use types::*;

use super::view::{ConversationView, ConversationViews, ViewObserver};
use crate::{
    storage::{StorageFuture, line::eager},
    *,
};
use parking_lot::Mutex;
use pi_chord::{Watch, delta::Op};
use std::{future::Future, sync::Arc};

struct EventObserver {
    snapshot: Arc<SnapshotEvent>,
    watch: Watch<EventBatch>,
    current: Arc<Mutex<SnapshotEvent>>,
    translator: Mutex<EventTranslator>,
}
impl ViewObserver for EventObserver {
    fn publication(
        &self,
        before: Arc<ConversationView>,
        after: Arc<ConversationView>,
        ops: &[Op],
        publication: &CommitPublication,
        context: &Context,
    ) -> Result<()> {
        *self.current.lock() = snapshot_of(after.revision())?;
        let events = self.translator.lock().translate(
            after.conversation.id,
            before.revision(),
            after.revision(),
            ops,
            publication,
        )?;
        if !events.is_empty() {
            self.watch
                .advance(Arc::new(events), Vec::new(), context.clone());
        }
        Ok(())
    }
    fn end(&self, failure: Option<Error>) -> Result<()> {
        self.watch.end_with_error(failure.map(Into::into));
        Ok(())
    }
}
#[derive(Clone)]
pub struct AgentEventStream {
    pub snapshot: Arc<SnapshotEvent>,
    pub watch: Watch<EventBatch>,
    // Keep the shared mount alive while a stream handle remains live.
    _views: ConversationViews,
}
impl AgentEventStream {
    pub fn start<F, Fut>(&self, listener: F) -> Result<()>
    where
        F: Fn(Arc<EventBatch>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = std::result::Result<(), String>> + Send + 'static,
    {
        self.watch
            .start(move |frame| listener(frame.value, frame.context))
            .map_err(Error::message)
    }
    pub fn stop(&self) -> pi_chord::WatchEnd {
        self.watch.stop()
    }
    pub async fn closed(&self) -> pi_chord::WatchEnd {
        self.watch.closed().await
    }
}
pub fn watch_events(
    views: &ConversationViews,
    conversation: ConversationId,
    context: &Context,
) -> StorageFuture<AgentEventStream> {
    let report = views.reporter();
    let context_for_scan = context.clone();
    let acquire = views.attach(
        conversation,
        move |initial, release, storage| async move {
            let query = TaskQuery {
                conversation_id: Some(conversation),
                kind: Some("pi.generation".into()),
                status: Some(TaskStatus::Completing),
                ..Default::default()
            };
            let mut cursor = None;
            let mut held = Vec::new();
            loop {
                let page = storage
                    .scan_tasks(&query, 100, cursor.as_ref(), &context_for_scan)
                    .await?;
                held.extend(page.items.iter().map(TaskRecord::id));
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
            let snapshot = Arc::new(snapshot_of(initial.revision())?);
            let current = Arc::new(Mutex::new(snapshot.as_ref().clone()));
            let replacement = current.clone();
            let watch = Watch::with_source(
                Arc::new(Vec::new()),
                move || release(),
                report,
                move || Arc::new(vec![AgentEvent::Snapshot(replacement.lock().clone())]),
            );
            Ok(Arc::new(EventObserver {
                snapshot,
                watch,
                current,
                translator: Mutex::new(EventTranslator::new(held)),
            }))
        },
        context,
    );
    let views = views.clone();
    let context = context.clone();
    eager(async move {
        let observer = acquire.await?.observer;
        if let Err(error) = context.check() {
            observer.watch.cancel();
            return Err(error.into());
        }
        if let Some(signal) = context.abort_signal() {
            observer
                .watch
                .observe_cancellation(signal.clone())
                .map_err(Error::message)?;
        }
        Ok(AgentEventStream {
            snapshot: observer.snapshot.clone(),
            watch: observer.watch.clone(),
            _views: views,
        })
    })
}
