//! A shared, observer-owned view of live tasks, advanced by durable publications.
use super::{
    reader::ReaderSubscription,
    scheduler::{BlockedReason, BlockedSubject},
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
    delta::{Op, Seg},
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum TaskGraphState {
    Pending {
        phase: JsonString,
    },
    Running {
        phase: JsonString,
    },
    Waiting {
        phase: JsonString,
        on: Vec<TaskId>,
        policy: JoinPolicy,
    },
    Sleeping {
        phase: JsonString,
        until: f64,
    },
    Completing {
        outcome: TaskGraphOutcome,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskGraphOutcome {
    Completed,
    Failed,
    Aborted,
    Orphaned,
    Faulted,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskGraphNode {
    pub id: TaskId,
    pub kind: JsonString,
    pub conversation_id: ConversationId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<TaskId>,
    pub background: bool,
    pub abort_requested: bool,
    pub state: TaskGraphState,
    pub conversations: Vec<ConversationId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedReason>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TaskGraph {
    pub tasks: BTreeMap<TaskId, Arc<TaskGraphNode>>,
}
pub type TaskGraphWatch = Watch<TaskGraph>;
type Listener = Arc<dyn Fn() -> Result<()> + Send + Sync>;
#[derive(Clone)]
pub struct TaskGraphBlocking {
    pub blocked: Arc<dyn Fn(&BlockedSubject) -> Option<BlockedReason> + Send + Sync>,
    pub subscribe: Arc<dyn Fn(Listener) -> Result<ReaderSubscription> + Send + Sync>,
}
struct Mount {
    value: Arc<TaskGraph>,
    observers: IndexMap<u64, TaskGraphWatch>,
    subjects: IndexMap<TaskId, BlockedSubject>,
}
struct Inner {
    session: Session,
    storage: Arc<dyn Storage>,
    blocking: TaskGraphBlocking,
    mount: Mutex<Option<Arc<Mutex<Mount>>>>,
    closed: AtomicBool,
    next: AtomicU64,
    subscriptions: Mutex<Option<(Subscription, Subscription, ReaderSubscription)>>,
}
#[derive(Clone)]
pub struct TaskGraphView {
    inner: Arc<Inner>,
}
impl TaskGraphView {
    pub fn new(
        session: Session,
        storage: Arc<dyn Storage>,
        blocking: TaskGraphBlocking,
    ) -> Result<Self> {
        let graph = Self {
            inner: Arc::new(Inner {
                session: session.clone(),
                storage,
                blocking: blocking.clone(),
                mount: Mutex::new(None),
                closed: AtomicBool::new(false),
                next: AtomicU64::new(1),
                subscriptions: Mutex::new(None),
            }),
        };
        let weak = Arc::downgrade(&graph.inner);
        let commits = session.subscribe_commits(move |publication, context| {
            if let Some(inner) = weak.upgrade() {
                TaskGraphView { inner }.advance(publication, context)?;
            }
            Ok(())
        })?;
        let weak = Arc::downgrade(&graph.inner);
        let blocked = (blocking.subscribe)(Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                TaskGraphView { inner }.refresh_blocked();
            }
            Ok(())
        }))?;
        let weak = Arc::downgrade(&graph.inner);
        let close = session.subscribe_close(move || {
            if let Some(inner) = weak.upgrade() {
                inner.closed.store(true, Ordering::Release);
                let mount = inner.mount.lock().take();
                if let Some(mount) = mount {
                    let observers = mount.lock().observers.values().cloned().collect::<Vec<_>>();
                    for observer in observers {
                        observer.end_with_error(inner.session.failure().map(Into::into));
                    }
                }
            }
        })?;
        *graph.inner.subscriptions.lock() = Some((commits, close, blocked));
        Ok(graph)
    }
    pub fn watch(&self, context: &Context) -> StorageFuture<TaskGraphWatch> {
        let graph = self.clone();
        let read_context = context.clone();
        let read = self.inner.session.read_on_line(move || async move {
            let existing = graph.inner.mount.lock().clone();
            let mount = match existing {
                Some(mount) => mount,
                None => Arc::new(Mutex::new(graph.build(&read_context).await?)),
            };
            let value = mount.lock().value.clone();
            let id = graph.inner.next.fetch_add(1, Ordering::Relaxed);
            let weak_graph = Arc::downgrade(&graph.inner);
            let weak_mount = Arc::downgrade(&mount);
            let report = graph.inner.session.clone();
            let observer = Watch::with_frames(
                value,
                move || {
                    if let (Some(graph), Some(mount)) = (weak_graph.upgrade(), weak_mount.upgrade())
                    {
                        // Detach may race acquisition; use the same lock order there.
                        let mut current = graph.mount.lock();
                        let mut held = mount.lock();
                        held.observers.shift_remove(&id);
                        if held.observers.is_empty()
                            && current
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &mount))
                        {
                            *current = None;
                        }
                    }
                },
                move |error| report.report(Error::from(error)),
                |value| vec![Op::Replace(crate::wire::encode(value.as_ref()))],
            );
            let mut current = graph.inner.mount.lock();
            graph.inner.session.usable()?;
            if graph.inner.closed.load(Ordering::Acquire) {
                return Err(graph
                    .inner
                    .session
                    .failure()
                    .map_or_else(|| Error::message("Session is closed"), Error::failed));
            }
            read_context.check()?;
            *current = Some(mount.clone());
            mount.lock().observers.insert(id, observer.clone());
            Ok(observer)
        });
        let context = context.clone();
        eager(async move {
            let observer = read.await?;
            if let Err(error) = context.check() {
                observer.cancel();
                return Err(error.into());
            }
            if let Some(signal) = context.abort_signal() {
                observer
                    .observe_cancellation(signal.clone())
                    .map_err(Error::message)?;
            }
            Ok(observer)
        })
    }
    async fn build(&self, context: &Context) -> Result<Mount> {
        let mut records = Vec::new();
        for status in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::Waiting,
            TaskStatus::Sleeping,
            TaskStatus::Completing,
        ] {
            let mut cursor = None;
            loop {
                let page = self
                    .inner
                    .storage
                    .scan_tasks(
                        &TaskQuery {
                            status: Some(status),
                            ..Default::default()
                        },
                        256,
                        cursor.as_ref(),
                        context,
                    )
                    .await?;
                records.extend(page.items);
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        records.sort_by_key(TaskRecord::id);
        let mut graph = TaskGraph::default();
        let mut subjects = IndexMap::new();
        for record in records {
            let mut conversations = Vec::new();
            let mut cursor = None;
            loop {
                let page = self
                    .inner
                    .storage
                    .scan_conversations(
                        &ConversationQuery {
                            owner_task_id: Some(record.id()),
                            ..Default::default()
                        },
                        256,
                        cursor.as_ref(),
                        context,
                    )
                    .await?;
                conversations.extend(page.items.into_iter().map(|record| record.id));
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
            conversations.sort();
            let subject = BlockedSubject::from(&record);
            graph.tasks.insert(
                record.id(),
                Arc::new(node_of(
                    &record,
                    conversations,
                    (self.inner.blocking.blocked)(&subject),
                )?),
            );
            subjects.insert(record.id(), subject);
        }
        Ok(Mount {
            value: Arc::new(graph),
            subjects,
            observers: IndexMap::new(),
        })
    }
    fn refresh_blocked(&self) {
        if self.inner.closed.load(Ordering::Acquire) || self.inner.mount.lock().is_none() {
            return;
        }
        let graph = self.clone();
        // Failure after closing is expected; publication/listener failures are
        // reported by their source and never poison an otherwise healthy Session.
        drop(self.inner.session.read_on_line(move || async move {
            let mount = graph.inner.mount.lock().clone();
            let Some(mount) = mount else {
                return Ok(());
            };
            let (value, observers, ops) = {
                let (mut value, subjects) = {
                    let held = mount.lock();
                    (held.value.clone(), held.subjects.clone())
                };
                let mut ops = Vec::new();
                for (id, subject) in &subjects {
                    let blocked = (graph.inner.blocking.blocked)(subject);
                    if value.tasks.get(id).expect("live graph subject").blocked == blocked {
                        continue;
                    }
                    let node = Arc::make_mut(&mut value)
                        .tasks
                        .get_mut(id)
                        .expect("live graph subject");
                    Arc::make_mut(node).blocked = blocked;
                    let path = path(*id, Some("blocked"));
                    ops.push(match blocked {
                        None => Op::Delete(path),
                        Some(blocked) => Op::Set(path, crate::wire::encode(&blocked)),
                    });
                }
                prepare_publication(&mut mount.lock(), value, ops)
            };
            publish(value, observers, ops, &Context::default());
            Ok(())
        }));
    }
    fn advance(&self, publication: &CommitPublication, context: &Context) -> Result<()> {
        if !publication.changes.iter().any(|change| {
            matches!(
                change,
                CommitChange::Task(_)
                    | CommitChange::Conversation(ConversationRecord { owner: Some(_), .. })
            )
        }) {
            return Ok(());
        }
        let mount = self.inner.mount.lock().clone();
        let Some(mount) = mount else {
            return Ok(());
        };
        let (value, observers, ops) = {
            let (mut value, mut subjects) = {
                let held = mount.lock();
                (held.value.clone(), held.subjects.clone())
            };
            let mut ops = Vec::new();
            for change in &publication.changes {
                let CommitChange::Task(record) = change else {
                    continue;
                };
                let id = record.id();
                if record.state.status() == TaskStatus::Terminal {
                    subjects.shift_remove(&id);
                    if value.tasks.contains_key(&id) {
                        Arc::make_mut(&mut value).tasks.remove(&id);
                        ops.push(Op::Delete(path(id, None)));
                    }
                    continue;
                }
                let subject = BlockedSubject::from(record.as_ref());
                let conversations = value
                    .tasks
                    .get(&id)
                    .map(|node| node.conversations.clone())
                    .unwrap_or_default();
                let next = node_of(
                    record,
                    conversations,
                    (self.inner.blocking.blocked)(&subject),
                )?;
                subjects.insert(id, subject);
                if value.tasks.get(&id).map(Arc::as_ref) == Some(&next) {
                    continue;
                }
                ops.push(Op::Set(path(id, None), crate::wire::encode(&next)));
                Arc::make_mut(&mut value).tasks.insert(id, Arc::new(next));
            }
            let mut created = IndexMap::<TaskId, Vec<ConversationId>>::new();
            for change in &publication.changes {
                let CommitChange::Conversation(record) = change else {
                    continue;
                };
                if let Some(owner) = &record.owner
                    && value.tasks.contains_key(&owner.task_id)
                {
                    created.entry(owner.task_id).or_default().push(record.id);
                }
            }
            for (id, created) in created {
                let node = Arc::make_mut(&mut value)
                    .tasks
                    .get_mut(&id)
                    .expect("live conversation owner");
                let node = Arc::make_mut(node);
                node.conversations.extend(created);
                node.conversations.sort();
                ops.push(Op::Set(
                    path(id, Some("conversations")),
                    crate::wire::encode(&node.conversations),
                ));
            }
            let mut mount = mount.lock();
            mount.subjects = subjects;
            prepare_publication(&mut mount, value, ops)
        };
        publish(value, observers, ops, context);
        Ok(())
    }
}
fn path(id: TaskId, field: Option<&str>) -> Vec<Seg> {
    let mut path = vec![Seg::Key("tasks".into()), Seg::Key(id.to_string().into())];
    if let Some(field) = field {
        path.push(Seg::Key(field.into()));
    }
    path
}
fn prepare_publication(
    mount: &mut Mount,
    value: Arc<TaskGraph>,
    ops: Vec<Op>,
) -> (Arc<TaskGraph>, Vec<TaskGraphWatch>, Vec<Op>) {
    if !ops.is_empty() {
        mount.value = value;
    }
    (
        mount.value.clone(),
        mount.observers.values().cloned().collect(),
        ops,
    )
}
fn publish(value: Arc<TaskGraph>, observers: Vec<TaskGraphWatch>, ops: Vec<Op>, context: &Context) {
    if ops.is_empty() {
        return;
    }
    let context = without_abort_signal(context);
    for observer in observers {
        observer.advance(value.clone(), ops.clone(), context.clone());
    }
}
fn node_of(
    record: &TaskRecord,
    conversations: Vec<ConversationId>,
    blocked: Option<BlockedReason>,
) -> Result<TaskGraphNode> {
    let phase = || {
        record
            .state
            .checkpoint()
            .and_then(|checkpoint| crate::wire::get(checkpoint, "phase"))
            .ok_or_else(|| Error::message("Task checkpoint has no phase"))
            .and_then(|phase| crate::wire::decode(phase).map_err(Error::message))
    };
    let state = match &record.state {
        TaskState::Pending { .. } => TaskGraphState::Pending { phase: phase()? },
        TaskState::Running { .. } => TaskGraphState::Running { phase: phase()? },
        TaskState::Waiting { on, policy, .. } => TaskGraphState::Waiting {
            phase: phase()?,
            on: on.clone(),
            policy: *policy,
        },
        TaskState::Sleeping { until, .. } => TaskGraphState::Sleeping {
            phase: phase()?,
            until: *until,
        },
        TaskState::Completing { outcome } | TaskState::Terminal { outcome } => {
            TaskGraphState::Completing {
                outcome: match outcome {
                    TaskOutcome::Completed { .. } => TaskGraphOutcome::Completed,
                    TaskOutcome::Failed { .. } => TaskGraphOutcome::Failed,
                    TaskOutcome::Aborted { .. } => TaskGraphOutcome::Aborted,
                    TaskOutcome::Orphaned { .. } => TaskGraphOutcome::Orphaned,
                    TaskOutcome::Faulted { .. } => TaskGraphOutcome::Faulted,
                },
            }
        }
    };
    Ok(TaskGraphNode {
        id: record.id(),
        kind: record.info.kind.clone(),
        conversation_id: record.info.conversation_id,
        owner: record.info.owner,
        background: record.info.background,
        abort_requested: record.info.abort_requested,
        state,
        conversations,
        blocked,
    })
}
