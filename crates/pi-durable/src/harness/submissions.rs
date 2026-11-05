//! Admission, durable receipts and queued withdrawal on the shared Session line.
use super::{
    generation::start_run,
    inbox::*,
    live::live_doc,
    types::*,
    util::{Waiters, closed_error},
};
use crate::{
    storage::{StorageFuture, line::eager, ready},
    wire, *,
};
use parking_lot::Mutex;
use pi_chord::context::with_telemetry;
use pi_telemetry::{ContextRef, SpanLink, SpanOptions, SpanRef, SpanStatus, TelemetryContextExt};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub struct SubmissionsOptions {
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
    pub queue_modes: Arc<dyn Fn() -> Result<QueueModes> + Send + Sync>,
    pub resume: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    pub telemetry: ContextRef,
}
struct Inner {
    session: Session,
    storage: Arc<dyn Storage>,
    options: SubmissionsOptions,
    closed: AtomicBool,
    waiters: Waiters<SubmissionId, SettledSubmissionRecord>,
    subscriptions: Mutex<Option<(session::Subscription, session::Subscription)>>,
}
#[derive(Clone)]
pub struct Submissions {
    inner: Arc<Inner>,
}
#[derive(Clone)]
pub struct Submission {
    pub id: SubmissionId,
    submissions: Submissions,
    // Public Harness handles also own the services used by later task phases.
    // The core service never stores handles, so this cannot form a cycle.
    harness: Option<super::Harness>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionAbort {
    Aborted,
    AlreadyPlaced,
    Settled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdmissionStatus {
    Queued,
    Placed,
    Done,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    pub id: SubmissionId,
    pub status: AdmissionStatus,
}
impl Submissions {
    pub fn new(session: Session, options: SubmissionsOptions) -> Result<Self> {
        let result = Self {
            inner: Arc::new(Inner {
                storage: session.guarded_storage(),
                session,
                options,
                closed: AtomicBool::new(false),
                waiters: Waiters::default(),
                subscriptions: Mutex::new(None),
            }),
        };
        let weak = Arc::downgrade(&result.inner);
        let commits = result
            .inner
            .session
            .subscribe_internal(move |publication, _| {
                if let Some(inner) = weak.upgrade() {
                    for change in &publication.changes {
                        if let CommitChange::Submission(record) = change
                            && let Ok(settled) = SettledSubmissionRecord::try_from(record.clone())
                        {
                            inner.waiters.resolve(&record.id(), settled);
                        }
                    }
                }
                Ok(())
            })?;
        let weak = Arc::downgrade(&result.inner);
        let close = result.inner.session.subscribe_close(move || {
            if let Some(inner) = weak.upgrade() {
                inner.closed.store(true, Ordering::Release);
                inner.waiters.reject_all(closed_error(&inner.session));
            }
        })?;
        *result.inner.subscriptions.lock() = Some((commits, close));
        Ok(result)
    }
    fn handle(&self, id: SubmissionId) -> Submission {
        Submission {
            id,
            submissions: self.clone(),
            harness: None,
        }
    }
    pub fn submit(
        &self,
        conversation: ConversationId,
        draft: SubmissionDraft,
        context: &Context,
    ) -> StorageFuture<Submission> {
        if let Err(error) = (self.inner.options.resume)() {
            return ready(Err(error));
        }
        let parent = if matches!(draft, SubmissionDraft::Input { .. }) {
            self.inner.options.telemetry.clone()
        } else {
            context.telemetry().clone()
        };
        let options=SpanOptions::new("pi.durable.submit").attributes(pi_telemetry::attributes! {"submission.type"=>draft.kind(),"conversation.id"=>conversation.get() as f64});
        let service = self.clone();
        let context = context.clone();
        let operation=parent.start_span(options,move |span| {
            if matches!(draft,SubmissionDraft::Input {..}) {span.add_link(SpanLink::new(context.telemetry().clone()));}
            let span_context=with_telemetry(span.clone(),&context);let admission_span=span.clone();let admission=service.clone();
            let operation=service.inner.session.commit(move |tx| async move {
                let now=(admission.inner.options.now)();
                let modes=(admission.inner.options.queue_modes)()?;
                admit_submission(&tx,conversation,&draft,now,modes,Some(&admission_span)).await
            },&span_context);
            async move {
                match operation.await {
                    Ok(admission)=>{let status=match admission.status {AdmissionStatus::Queued=>"queued",AdmissionStatus::Placed=>"placed",AdmissionStatus::Done=>"done"};
                        span.set_attributes(pi_telemetry::attributes! {"submission.id"=>admission.id.get() as f64,"result"=>status});Ok(service.handle(admission.id))}
                    Err(error)=>{span.set_status(SpanStatus::Error(None));Err(error)}
                }
            }
        });
        eager(operation)
    }
    pub fn get(&self, id: SubmissionId, context: &Context) -> StorageFuture<Option<Submission>> {
        let service = self.clone();
        let storage = self.inner.storage.clone();
        let context = context.clone();
        let read = self
            .inner
            .session
            .read_on_line(move || async move { storage.submission(id, &context).await });
        eager(async move { Ok(read.await?.map(|record| service.handle(record.id()))) })
    }
    pub fn status(&self, id: SubmissionId, context: &Context) -> StorageFuture<SubmissionRecord> {
        let storage = self.inner.storage.clone();
        let context = context.clone();
        self.inner.session.read_on_line(move || async move {
            storage
                .submission(id, &context)
                .await?
                .ok_or_else(|| Error::message(format!("Submission {id} does not exist")))
        })
    }
    pub fn wait(
        &self,
        id: SubmissionId,
        context: &Context,
    ) -> StorageFuture<SettledSubmissionRecord> {
        if let Err(error) = (self.inner.options.resume)() {
            return ready(Err(error));
        }
        let service = self.clone();
        let context = context.clone();
        let found = self.inner.session.read_on_line(move || async move {
            let record = service
                .inner
                .storage
                .submission(id, &context)
                .await?
                .ok_or_else(|| Error::message(format!("Submission {id} does not exist")))?;
            if let Ok(settled) = SettledSubmissionRecord::try_from(record) {
                return Ok(ready(Ok(settled)));
            }
            if service.inner.closed.load(Ordering::Acquire) {
                return Err(closed_error(&service.inner.session));
            }
            Ok(service.inner.waiters.add(id, &context))
        });
        let owner = self.clone();
        eager(async move {
            let receipt = found.await?.await?;
            owner.inner.session.publication_settled().await;
            Ok(receipt)
        })
    }
    /// None is the source API's "not_found", including a conversation mismatch.
    pub fn abort(
        &self,
        id: SubmissionId,
        context: &Context,
        conversation: Option<ConversationId>,
    ) -> StorageFuture<Option<SubmissionAbort>> {
        self.inner.session.commit(
            move |tx| async move {
                let Some(record) = tx.submission(id).await?.filter(|record| {
                    conversation
                        .is_none_or(|conversation| record.info.conversation_id == conversation)
                }) else {
                    return Ok(None);
                };
                if record.state.status() == SubmissionStatus::Queued {
                    tx.settle_submission(
                        id,
                        SubmissionSettlement::Unanswered {
                            reason: "aborted".into(),
                            detail: None,
                        },
                    )?;
                    remove_inbox_item(&tx, record.info.conversation_id, id).await?;
                    return Ok(Some(SubmissionAbort::Aborted));
                }
                Ok(Some(if record.state.status() == SubmissionStatus::Placed {
                    SubmissionAbort::AlreadyPlaced
                } else {
                    SubmissionAbort::Settled
                }))
            },
            context,
        )
    }
}
impl Submission {
    pub(crate) fn retaining(mut self, harness: super::Harness) -> Self {
        self.harness = Some(harness);
        self
    }
    fn keep_alive<T: Send + 'static>(&self, operation: StorageFuture<T>) -> StorageFuture<T> {
        let Some(harness) = self.harness.clone() else {
            return operation;
        };
        crate::storage::line::eager_on(&self.submissions.inner.session, async move {
            let result = operation.await;
            drop(harness);
            result
        })
    }
    pub fn status(&self, context: &Context) -> StorageFuture<SubmissionRecord> {
        self.keep_alive(self.submissions.status(self.id, context))
    }
    pub fn wait(&self, context: &Context) -> StorageFuture<SettledSubmissionRecord> {
        self.keep_alive(self.submissions.wait(self.id, context))
    }
    pub fn abort(&self, context: &Context) -> StorageFuture<SubmissionAbort> {
        let operation = self.submissions.abort(self.id, context, None);
        let id = self.id;
        self.keep_alive(eager(async move {
            operation
                .await?
                .ok_or_else(|| Error::message(format!("Submission {id} does not exist")))
        }))
    }
}
pub async fn admit_submission(
    tx: &Transaction,
    conversation: ConversationId,
    draft: &SubmissionDraft,
    now: f64,
    modes: QueueModes,
    span: Option<&SpanRef>,
) -> Result<Admission> {
    if let SubmissionDraft::Input {
        entry: Some(entry), ..
    } = draft
        && (entry.kind.units().is_empty()
            || entry
                .kind
                .units()
                .starts_with(&[b'p' as u16, b'i' as u16, b'.' as u16]))
    {
        return Err(Error::from(pi_chord::delta::Error::Type(format!(
            "An input entry's kind must be a non-empty string not starting with \"pi.\", got {}",
            entry.kind
        ))));
    }
    if let Some(request) = draft.request_id()
        && let Some(existing) = tx
            .submission_by_request(conversation, request.clone())
            .await?
    {
        let kind = match existing.state {
            SubmissionState::InputQueued
            | SubmissionState::InputPlaced { .. }
            | SubmissionState::InputDone { .. }
            | SubmissionState::InputUnanswered { .. } => "input",
            _ => "write",
        };
        if kind != draft.kind() {
            return Err(Error::message(format!(
                "Request {request} already identifies a submission of type {kind}"
            )));
        }
        let status = match existing.state.status() {
            SubmissionStatus::Queued => AdmissionStatus::Queued,
            SubmissionStatus::Placed => AdmissionStatus::Placed,
            _ => AdmissionStatus::Done,
        };
        return Ok(Admission {
            id: existing.id(),
            status,
        });
    }
    let trace = span
        .and_then(|span| {
            span.propagation()
                .and_then(|propagation| propagation.inject(span.as_ref()))
        })
        .map(|carrier| JsonObject::try_from_value(wire::encode(&carrier)).map_err(Error::message))
        .transpose()?;
    let create = |state| SubmissionCreate {
        conversation_id: conversation,
        request_id: draft.request_id().cloned(),
        trace: trace.clone(),
        state,
    };
    if let SubmissionDraft::Write { entry, .. } = draft
        && is_display_only(entry)
        && !has_queued_reset(&tx.doc(&inbox_doc(), conversation).await?.read()?.items)
    {
        let entry = tx.append_entry(conversation, entry.clone()).await?;
        return Ok(Admission {
            id: tx
                .create_submission(create(SubmissionState::WriteDone { entry: entry.id }))
                .await?
                .id(),
            status: AdmissionStatus::Done,
        });
    }
    let live = tx.doc(&live_doc(), conversation).await?;
    let run = live.read()?.run;
    let busy = run.is_some();
    if let SubmissionDraft::Input {
        when_busy: Some(WhenBusy::Steer),
        ..
    } = draft
        && let (Some(run), Some(span)) = (&run, span)
        && let Some(propagation) = span.propagation()
        && let Some(trace) = tx.task(run.task_id).await?.and_then(|task| task.info.trace)
    {
        let carrier: pi_telemetry::TelemetryCarrier =
            wire::decode(&trace.into_value()).map_err(Error::message)?;
        span.add_link(SpanLink::new(propagation.extract(Some(&carrier))));
    }
    if busy
        && matches!(
            draft,
            SubmissionDraft::Input {
                when_busy: Some(WhenBusy::Reject),
                ..
            }
        )
    {
        return Err(Error::busy(conversation));
    }
    let mut boundary = if busy {
        None
    } else {
        Some(prepare_boundary(tx, conversation, modes).await?)
    };
    let queued = match &boundary {
        None => true,
        Some(boundary) => !boundary.inbox.read()?.items.is_empty(),
    };
    if queued {
        let queued = if matches!(draft, SubmissionDraft::Input { .. }) {
            SubmissionState::InputQueued
        } else {
            SubmissionState::WriteQueued
        };
        let id = tx.create_submission(create(queued)).await?.id();
        let inbox = if let Some(boundary) = &boundary {
            boundary.inbox.clone()
        } else {
            tx.doc(&inbox_doc(), conversation).await?
        };
        let item = match draft {
            SubmissionDraft::Write { entry, .. } => InboxItem::Write {
                id,
                entry: entry.clone(),
            },
            SubmissionDraft::Input {
                content,
                when_busy,
                entry,
                ..
            } => InboxItem::Input {
                id,
                mode: if *when_busy == Some(WhenBusy::Steer) {
                    InputMode::Steer
                } else {
                    InputMode::FollowUp
                },
                content: content.clone(),
                entry: entry.clone(),
            },
        };
        push_inbox_item(&inbox, item)?;
        let Some(boundary) = &mut boundary else {
            return Ok(Admission {
                id,
                status: AdmissionStatus::Queued,
            });
        };
        let users = apply_boundary(tx, boundary, BoundaryAt::Final, now)
            .await?
            .users;
        let status = if matches!(draft, SubmissionDraft::Write { .. }) {
            AdmissionStatus::Done
        } else if users.last() == Some(&id) {
            AdmissionStatus::Placed
        } else {
            AdmissionStatus::Queued
        };
        if !users.is_empty() {
            start_run(tx, conversation, &live, users).await?;
        }
        return Ok(Admission { id, status });
    }
    match draft {
        SubmissionDraft::Write { entry, .. } => {
            let boundary = boundary.as_ref().expect("idle boundary");
            let state = if is_stale(boundary, entry) {
                SubmissionState::WriteUnanswered {
                    reason: "stale".into(),
                    detail: None,
                }
            } else {
                SubmissionState::WriteDone {
                    entry: tx.append_entry(conversation, entry.clone()).await?.id,
                }
            };
            Ok(Admission {
                id: tx.create_submission(create(state)).await?.id(),
                status: AdmissionStatus::Done,
            })
        }
        SubmissionDraft::Input { content, entry, .. } => {
            let entry =
                append_input(tx, conversation, content.clone(), entry.as_ref(), now).await?;
            let id = tx
                .create_submission(create(SubmissionState::InputPlaced { entry: entry.id }))
                .await?
                .id();
            start_run(tx, conversation, &live, vec![id]).await?;
            Ok(Admission {
                id,
                status: AdmissionStatus::Placed,
            })
        }
    }
}
