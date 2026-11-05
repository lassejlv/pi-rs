//! Native conversation view and event acquisition for unmodified stock tests.
use super::*;

#[derive(Default)]
pub(super) struct ViewBridge {
    subscriptions: Mutex<HashMap<u64, Subscription>>,
    attachments: Mutex<HashMap<u64, pi_durable::harness::view::DetachView>>,
    pub reads: AtomicU64,
    pub event_reads: AtomicU64,
    pub event_frames: AtomicU64,
}
struct HostViewObserver {
    hub: std::sync::Weak<Hub>,
    callback: u64,
}
impl pi_durable::harness::view::ViewObserver for HostViewObserver {
    fn publication(
        &self,
        before: Arc<pi_durable::harness::view::ConversationView>,
        after: Arc<pi_durable::harness::view::ConversationView>,
        ops: &[pi_chord::delta::Op],
        publication: &CommitPublication,
        _context: &Context,
    ) -> Result<()> {
        if let Some(hub) = self.hub.upgrade() {
            hub.sync(
                "view.publication",
                object([
                    ("callback", encode(&self.callback)),
                    ("before", hub.shared_revision(before.revision())),
                    ("after", hub.shared_revision(after.revision())),
                    ("ops", encode(&ops)),
                    ("publication", hub.native_publication(publication)),
                ]),
            )?;
        }
        Ok(())
    }
    fn end(&self, failure: Option<Error>) -> Result<()> {
        if let Some(hub) = self.hub.upgrade() {
            hub.sync(
                "view.end",
                object([
                    ("callback", encode(&self.callback)),
                    (
                        "failure",
                        failure.as_ref().map_or(JsonValue::Null, error_value),
                    ),
                ]),
            )?;
        }
        Ok(())
    }
}
impl Hub {
    pub(super) fn view_request(
        self: &Arc<Self>,
        method: &str,
        request: JsonValue,
        context: Context,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        if method == "view.unsubscribeCommits" {
            self.views
                .subscriptions
                .lock()
                .remove(&field(&request, "subscription")?);
            return Ok(Box::pin(async { Ok(None) }));
        }
        if method == "view.detachObserver" {
            if let Some(detach) = self
                .views
                .attachments
                .lock()
                .remove(&field(&request, "attachment")?)
            {
                detach();
            }
            return Ok(Box::pin(async { Ok(None) }));
        }
        if method == "view.subscribeClose" {
            let callback: u64 = field(&request, "callback")?;
            let weak = Arc::downgrade(self);
            let subscription = self.session(&request)?.subscribe_close_async(move || {
                if let Some(hub) = weak.upgrade() {
                    let result = hub.sync("session.closeListener", encode(&callback))?;
                    if let Some(pending) = result["pending"].as_u64() {
                        let operation = hub.call("commit.listenerResult", encode(&pending));
                        return Ok(Box::pin(async move {
                            operation.await?;
                            Ok(())
                        }));
                    }
                }
                Ok(Box::pin(async { Ok(()) }))
            })?;
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            self.views.subscriptions.lock().insert(id, subscription);
            return Ok(Box::pin(async move { Ok(Some(encode(&id))) }));
        }
        if method == "view.subscribeCommits" {
            let callback: u64 = field(&request, "callback")?;
            let weak = Arc::downgrade(self);
            let subscription =
                self.session(&request)?
                    .subscribe_commits_async(move |publication, _| {
                        if let Some(hub) = weak.upgrade() {
                            let result = hub.sync(
                                "commit.publication",
                                object([
                                    ("callback", encode(&callback)),
                                    ("publication", hub.native_publication(publication)),
                                ]),
                            )?;
                            if let Some(pending) = result["pending"].as_u64() {
                                let operation = hub.call("commit.listenerResult", encode(&pending));
                                return Ok(Box::pin(async move {
                                    operation.await?;
                                    Ok(())
                                }));
                            }
                        }
                        Ok(Box::pin(async { Ok(()) }))
                    })?;
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            self.views.subscriptions.lock().insert(id, subscription);
            return Ok(Box::pin(async move { Ok(Some(encode(&id))) }));
        }
        if method == "view.observeCommits" {
            let callback: u64 = field(&request, "callback")?;
            let weak = Arc::downgrade(self);
            let subscription =
                self.session(&request)?
                    .subscribe_internal(move |publication, _| {
                        if let Some(hub) = weak.upgrade() {
                            hub.sync(
                                "commit.publication",
                                object([
                                    ("callback", encode(&callback)),
                                    ("publication", hub.native_publication(publication)),
                                ]),
                            )?;
                        }
                        Ok(())
                    })?;
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            self.views.subscriptions.lock().insert(id, subscription);
            return Ok(Box::pin(async move { Ok(Some(encode(&id))) }));
        }
        let views = self.facade(&request)?.views().clone();
        self.views.reads.fetch_add(1, Ordering::Relaxed);
        let conversation = field(&request, "conversation")?;
        if method == "view.attach" {
            let callback: u64 = field(&request, "callback")?;
            let hub = self.clone();
            let attachment = self.next.fetch_add(1, Ordering::Relaxed);
            let create_context = context.clone();
            let acquire = views.attach(
                conversation,
                move |value, detach, _| async move {
                    hub.views.attachments.lock().insert(attachment, detach);
                    let result = hub
                        .call_context(
                            "view.create",
                            object([
                                ("callback", encode(&callback)),
                                ("attachment", encode(&attachment)),
                                ("value", hub.shared_revision(value.revision())),
                            ]),
                            &create_context,
                        )
                        .await?;
                    let callback = result
                        .and_then(|value| value.as_u64())
                        .ok_or_else(|| Error::message("View factory returned no observer"))?;
                    Ok(Arc::new(HostViewObserver {
                        hub: Arc::downgrade(&hub),
                        callback,
                    }))
                },
                &context,
            );
            return Ok(Box::pin(async move {
                let held = acquire.await?;
                Ok(Some(object([
                    ("observer", encode(&held.observer.callback)),
                    ("attachment", encode(&attachment)),
                ])))
            }));
        }
        if method == "view.events" {
            self.views.event_reads.fetch_add(1, Ordering::Relaxed);
            let acquire = pi_durable::harness::events::watch_events(&views, conversation, &context);
            let hub = self.clone();
            return Ok(Box::pin(async move {
                let stream = acquire.await?;
                let snapshot = encode(stream.snapshot.as_ref());
                let mut descriptor = hub.register_watch(graph_bridge::ValueWatch::Events(stream));
                descriptor
                    .as_object_mut()
                    .unwrap()
                    .insert("eventSnapshot".into(), snapshot);
                Ok(Some(descriptor))
            }));
        }
        if method == "view.watch" {
            let acquire = views.watch(conversation, &context);
            let hub = self.clone();
            return Ok(Box::pin(async move {
                Ok(Some(hub.register_watch(
                    graph_bridge::ValueWatch::Conversation(acquire.await?),
                )))
            }));
        }
        Err(Error::message(format!("Unknown view method {method}")))
    }
}
