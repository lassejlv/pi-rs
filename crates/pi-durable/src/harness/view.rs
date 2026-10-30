//! Shared mounts of active transcript entries and built-in conversation documents.
use super::{
    agent::AGENT_DOC,
    context::{active_entries, capture_context_bounds},
    inbox::INBOX_DOC,
    live::LIVE_DOC,
    provider::PROVIDER_DOC,
    usage::USAGE_DOC,
};
use crate::{
    storage::{StorageFuture, line::eager},
    *,
};
use indexmap::IndexMap;
use parking_lot::Mutex;
use pi_chord::{
    Watch,
    context::without_abort_signal,
    delta::{Op, Path, Seg, SharedJson},
};
use serde::Serialize;
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[derive(Clone, Debug)]
pub struct ConversationView {
    pub conversation: Arc<ConversationRecord>,
    pub entries: Arc<Vec<Arc<EntryRecord>>>,
    pub docs: Arc<IndexMap<JsonString, Arc<JsonValue>>>,
    revision: SharedJson,
}
impl ConversationView {
    pub fn revision(&self) -> &SharedJson {
        &self.revision
    }
    pub fn from_parts(
        conversation: Arc<ConversationRecord>,
        entries: Arc<Vec<Arc<EntryRecord>>>,
        docs: Arc<IndexMap<JsonString, Arc<JsonValue>>>,
    ) -> Self {
        let mut value = crate::wire::object();
        crate::wire::put(&mut value, "conversation", &conversation);
        crate::wire::put(&mut value, "entries", &entries);
        value.as_object_mut().unwrap().insert(
            "docs".into(),
            JsonValue::Object(
                docs.iter()
                    .map(|(kind, value)| (kind.clone(), value.as_ref().clone()))
                    .collect::<pi_chord::json::Map>()
                    .into(),
            ),
        );
        Self {
            conversation,
            entries,
            docs,
            revision: SharedJson::new(value),
        }
    }
}
impl Serialize for ConversationView {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut value = crate::wire::object();
        crate::wire::put(&mut value, "conversation", &self.conversation);
        crate::wire::put(&mut value, "entries", &self.entries);
        value.as_object_mut().unwrap().insert(
            "docs".into(),
            JsonValue::Object(
                self.docs
                    .iter()
                    .map(|(kind, value)| (kind.clone(), value.as_ref().clone()))
                    .collect::<pi_chord::json::Map>()
                    .into(),
            ),
        );
        value.serialize(serializer)
    }
}
pub type ConversationViewWatch = Watch<ConversationView>;
pub type DetachView = Arc<dyn Fn() + Send + Sync>;
pub trait ViewObserver: Send + Sync + 'static {
    fn advance(
        &self,
        _value: Arc<ConversationView>,
        _ops: &[Op],
        _context: &Context,
    ) -> Result<()> {
        Ok(())
    }
    fn publication(
        &self,
        _before: Arc<ConversationView>,
        _after: Arc<ConversationView>,
        _ops: &[Op],
        _publication: &CommitPublication,
        _context: &Context,
    ) -> Result<()> {
        Ok(())
    }
    fn end(&self, failure: Option<Error>) -> Result<()>;
}
impl ViewObserver for ConversationViewWatch {
    fn advance(&self, value: Arc<ConversationView>, ops: &[Op], context: &Context) -> Result<()> {
        self.advance(value, ops.to_vec(), context.clone());
        Ok(())
    }
    fn end(&self, failure: Option<Error>) -> Result<()> {
        self.end_with_error(failure.map(Into::into));
        Ok(())
    }
}
pub struct ViewAttachment<O> {
    pub observer: Arc<O>,
    pub detach: DetachView,
}
struct Mount {
    value: Arc<ConversationView>,
    incarnations: IndexMap<JsonString, (DocumentId, u64)>,
    observers: IndexMap<u64, Arc<dyn ViewObserver>>,
}
struct Inner {
    session: Session,
    storage: Arc<dyn Storage>,
    mounts: Mutex<IndexMap<ConversationId, Arc<Mutex<Mount>>>>,
    closed: AtomicBool,
    next: AtomicU64,
    subscriptions: Mutex<Option<(Subscription, Subscription)>>,
}
#[derive(Clone)]
pub struct ConversationViews {
    inner: Arc<Inner>,
}
fn mounted() -> Vec<Arc<AnyDocDefinition>> {
    vec![
        AGENT_DOC.definition().clone(),
        LIVE_DOC.definition().clone(),
        INBOX_DOC.definition().clone(),
        PROVIDER_DOC.definition().clone(),
        USAGE_DOC.definition().clone(),
    ]
}
fn is_mounted(kind: &JsonString) -> bool {
    ["pi.agent", "pi.live", "pi.inbox", "pi.provider", "pi.usage"]
        .iter()
        .any(|name| kind.equals_str(name))
}
impl ConversationViews {
    pub fn new(session: Session) -> Result<Self> {
        let views = Self {
            inner: Arc::new(Inner {
                storage: session.guarded_storage(),
                session: session.clone(),
                mounts: Mutex::new(IndexMap::new()),
                closed: AtomicBool::new(false),
                next: AtomicU64::new(1),
                subscriptions: Mutex::new(None),
            }),
        };
        let weak = Arc::downgrade(&views.inner);
        let commits = session.subscribe_internal(move |publication, context| {
            if let Some(inner) = weak.upgrade() {
                ConversationViews { inner }.advance(publication, context)?;
            }
            Ok(())
        })?;
        let weak = Arc::downgrade(&views.inner);
        let closed = session.subscribe_close(move || {
            if let Some(inner) = weak.upgrade() {
                inner.closed.store(true, Ordering::Release);
                let mounts = std::mem::take(&mut *inner.mounts.lock());
                let views = ConversationViews { inner };
                for mount in mounts.into_values() {
                    let observers = mount.lock().observers.values().cloned().collect::<Vec<_>>();
                    for observer in observers {
                        views.contained(|| observer.end(views.inner.session.failure()));
                    }
                }
            }
        })?;
        *views.inner.subscriptions.lock() = Some((commits, closed));
        Ok(views)
    }
    pub fn report(&self, error: Error) {
        self.inner.session.report(error);
    }
    pub(crate) fn reporter(&self) -> impl Fn(pi_chord::WatchError) + Send + Sync + 'static {
        let weak = Arc::downgrade(&self.inner);
        move |error| {
            if let Some(inner) = weak.upgrade() {
                inner.session.report(Error::from(error));
            }
        }
    }
    fn contained(&self, invoke: impl FnOnce() -> Result<()>) {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(invoke)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.report(error),
            Err(panic) => self.report(crate::session::panic_error(panic)),
        }
    }
    pub fn attach<O, F, Fut>(
        &self,
        id: ConversationId,
        create: F,
        context: &Context,
    ) -> StorageFuture<ViewAttachment<O>>
    where
        O: ViewObserver,
        F: FnOnce(Arc<ConversationView>, DetachView, Arc<dyn Storage>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Arc<O>>> + Send + 'static,
    {
        let views = self.clone();
        let context = context.clone();
        let session = self.inner.session.clone();
        let acquire = self.inner.session.read_on_line(move || async move {
            let existing = views.inner.mounts.lock().get(&id).cloned();
            let mount = match existing {
                Some(mount) => mount,
                None => Arc::new(Mutex::new(views.build(id, &context).await?)),
            };
            let observer_id = views.inner.next.fetch_add(1, Ordering::Relaxed);
            let weak = Arc::downgrade(&views.inner);
            let weak_mount = Arc::downgrade(&mount);
            let detach: DetachView = Arc::new(move || {
                if let (Some(inner), Some(mount)) = (weak.upgrade(), weak_mount.upgrade()) {
                    let mut mounts = inner.mounts.lock();
                    let mut held = mount.lock();
                    held.observers.shift_remove(&observer_id);
                    if held.observers.is_empty()
                        && mounts
                            .get(&id)
                            .is_some_and(|current| Arc::ptr_eq(current, &mount))
                    {
                        mounts.shift_remove(&id);
                    }
                }
            });
            let value = mount.lock().value.clone();
            let observer = create(value, detach.clone(), views.inner.storage.clone()).await?;
            let mut mounts = views.inner.mounts.lock();
            views.inner.session.usable()?;
            if views.inner.closed.load(Ordering::Acquire) {
                return Err(views
                    .inner
                    .session
                    .failure()
                    .map_or_else(|| Error::message("Session is closed"), Error::failed));
            }
            context.check()?;
            mounts.insert(id, mount.clone());
            mount.lock().observers.insert(observer_id, observer.clone());
            Ok(ViewAttachment { observer, detach })
        });
        eager(async move {
            match acquire.await {
                Err(error)
                    if !matches!(error.kind(), ErrorKind::Aborted(_))
                        && session.usable().is_err() =>
                {
                    Err(super::util::closed_error(&session))
                }
                result => result,
            }
        })
    }
    pub fn watch(
        &self,
        id: ConversationId,
        context: &Context,
    ) -> StorageFuture<ConversationViewWatch> {
        let report = self.inner.session.clone();
        let acquire = self.attach(
            id,
            move |value, release, _| async move {
                Ok(Arc::new(Watch::with_frames(
                    value,
                    move || release(),
                    move |error| report.report(Error::from(error)),
                    |value| vec![Op::Replace(crate::wire::encode(value.as_ref()))],
                )))
            },
            context,
        );
        let context = context.clone();
        eager(async move {
            let watch = acquire.await?.observer.as_ref().clone();
            if let Err(error) = context.check() {
                watch.cancel();
                return Err(error.into());
            }
            if let Some(signal) = context.abort_signal() {
                watch
                    .observe_cancellation(signal.clone())
                    .map_err(Error::message)?;
            }
            Ok(watch)
        })
    }
    async fn build(&self, id: ConversationId, context: &Context) -> Result<Mount> {
        let conversation = self
            .inner
            .storage
            .conversation(id, context)
            .await?
            .ok_or_else(|| Error::message(format!("Conversation {id} does not exist")))?;
        let bounds = capture_context_bounds(self.inner.storage.as_ref(), id, context, None).await?;
        let entries = active_entries(self.inner.storage.as_ref(), id, bounds, context).await?;
        let mut docs = IndexMap::new();
        let mut incarnations = IndexMap::new();
        for definition in mounted() {
            let kind = definition.kind().clone();
            let address = DocumentAddress {
                kind: kind.clone(),
                scope: DocumentScope::Conversation {
                    conversation_id: id,
                },
                key: None,
            };
            if let Some((record, version, value)) = self
                .inner
                .session
                .document_on_line(definition, address, context)
                .await?
            {
                incarnations.insert(kind.clone(), (record.id(), version));
                docs.insert(kind, value);
            }
        }
        Ok(Mount {
            value: Arc::new(ConversationView::from_parts(
                Arc::new(conversation),
                Arc::new(entries),
                Arc::new(docs),
            )),
            incarnations,
            observers: IndexMap::new(),
        })
    }
    fn advance(&self, publication: &CommitPublication, context: &Context) -> Result<()> {
        let mounts = self
            .inner
            .mounts
            .lock()
            .iter()
            .map(|(id, mount)| (*id, mount.clone()))
            .collect::<Vec<_>>();
        for (id, mount) in mounts {
            if self.inner.closed.load(Ordering::Acquire) {
                break;
            }
            if !self
                .inner
                .mounts
                .lock()
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, &mount))
            {
                continue;
            }
            let (before, after, ops, observers) = {
                let mut mount = mount.lock();
                let before = mount.value.clone();
                let mut next = before.as_ref().clone();
                let mut doc_ops = Vec::new();
                let mut entry_ops = Vec::new();
                for change in &publication.changes {
                    match change {
                        CommitChange::Entry(entry) if entry.conversation_id == id => {
                            let entries = Arc::make_mut(&mut next.entries);
                            if let Some(target) = entry.head {
                                let kept = entries
                                    .iter()
                                    .position(|candidate| {
                                        candidate.head.is_none() && candidate.id >= target
                                    })
                                    .unwrap_or(entries.len());
                                entry_ops.push(Op::Splice(
                                    vec!["entries".into()],
                                    0,
                                    kept,
                                    vec![crate::wire::encode(entry)],
                                ));
                                entries.splice(..kept, [Arc::new(entry.clone())]);
                            } else {
                                entry_ops.push(Op::Splice(
                                    vec!["entries".into()],
                                    entries.len(),
                                    0,
                                    vec![crate::wire::encode(entry)],
                                ));
                                entries.push(Arc::new(entry.clone()));
                            }
                        }
                        CommitChange::Document(DocumentCommitChange::Document {
                            record,
                            conversation_id: Some(conversation),
                            version,
                            value,
                            ops,
                        }) if *conversation == id
                            && is_mounted(&record.create.kind)
                            && record.create.key.is_none() =>
                        {
                            let kind = &record.create.kind;
                            let path = vec!["docs".into(), Seg::Key(kind.clone())];
                            let mounted = mount.incarnations.get(kind).copied();
                            if value.is_null() {
                                if mounted.is_some_and(|(id, _)| id == record.id()) {
                                    mount.incarnations.shift_remove(kind);
                                    Arc::make_mut(&mut next.docs).shift_remove(kind);
                                    doc_ops.push(Op::Delete(path));
                                }
                            } else if mounted == version.map(|version| (record.id(), version)) {
                                if !ops.is_empty() {
                                    Arc::make_mut(&mut next.docs)
                                        .insert(kind.clone(), value.clone());
                                }
                                doc_ops.extend(ops.iter().map(|op| prefixed(op, &path)));
                            } else {
                                let version = version.ok_or_else(|| {
                                    Error::message("Live document publication has no version")
                                })?;
                                mount
                                    .incarnations
                                    .insert(kind.clone(), (record.id(), version));
                                Arc::make_mut(&mut next.docs).insert(kind.clone(), value.clone());
                                doc_ops.push(Op::Set(path, value.as_ref().clone()));
                            }
                        }
                        _ => {}
                    }
                }
                doc_ops.extend(entry_ops);
                if !doc_ops.is_empty() {
                    next.revision = before
                        .revision
                        .apply(&doc_ops)
                        .map_err(|error| Error::message(error.to_string()))?;
                    mount.value = Arc::new(next);
                }
                (
                    before,
                    mount.value.clone(),
                    doc_ops,
                    mount.observers.values().cloned().collect::<Vec<_>>(),
                )
            };
            let frame_context = without_abort_signal(context);
            if !ops.is_empty() {
                for observer in observers {
                    self.contained(|| observer.advance(after.clone(), &ops, &frame_context));
                }
            }
            let observers = mount.lock().observers.values().cloned().collect::<Vec<_>>();
            for observer in observers {
                self.contained(|| {
                    observer.publication(
                        before.clone(),
                        after.clone(),
                        &ops,
                        publication,
                        &frame_context,
                    )
                });
            }
        }
        Ok(())
    }
}
fn prefixed(op: &Op, prefix: &[Seg]) -> Op {
    let at = |path: &Path| {
        let mut result = prefix.to_vec();
        result.extend(path.iter().cloned());
        result
    };
    match op {
        Op::Replace(value) => Op::Set(prefix.to_vec(), value.clone()),
        Op::Set(path, value) => Op::Set(at(path), value.clone()),
        Op::Delete(path) => Op::Delete(at(path)),
        Op::Append(path, value) => Op::Append(at(path), value.clone()),
        Op::Truncate(path, count) => Op::Truncate(at(path), *count),
        Op::Splice(path, index, remove, items) => {
            Op::Splice(at(path), *index, *remove, items.clone())
        }
        Op::Move(path, order) => Op::Move(at(path), order.clone()),
    }
}
