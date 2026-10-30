//! Native watch transport shared by graphs and documents. JS supplies listeners.
use super::*;
use pi_chord::delta::{SharedJson, WeakSharedJson};
use pi_chord::{Watch, WatchEnd};
use pi_durable::harness::task_graph::TaskGraphWatch;
use std::{any::Any, sync::Weak};
#[derive(Default)]
pub(super) struct GraphBridge {
    watches: Mutex<HashMap<u64, ObservedWatch>>,
    revisions: Mutex<Vec<(u64, Weak<dyn Any + Send + Sync>)>>,
    shared: Mutex<Vec<(u64, WeakSharedJson)>>,
    pub reads: AtomicU64,
    pub frames: AtomicU64,
}
#[derive(Clone, Copy, Default)]
struct Activity {
    serial: u64,
    active: bool,
}
#[derive(Clone)]
pub(super) enum ValueWatch {
    Graph(TaskGraphWatch),
    Document(Watch),
    Conversation(pi_durable::harness::view::ConversationViewWatch),
    Events(pi_durable::harness::events::AgentEventStream),
}
impl ValueWatch {
    async fn delivery_idle(&self) {
        match self {
            Self::Graph(watch) => watch.delivery_idle().await,
            Self::Document(watch) => watch.delivery_idle().await,
            Self::Conversation(watch) => watch.delivery_idle().await,
            Self::Events(stream) => stream.watch.delivery_idle().await,
        }
    }
    async fn closed(&self) -> WatchEnd {
        match self {
            Self::Graph(watch) => watch.closed().await,
            Self::Document(watch) => watch.closed().await,
            Self::Conversation(watch) => watch.closed().await,
            Self::Events(stream) => stream.watch.closed().await,
        }
    }
    fn stop(&self) -> WatchEnd {
        match self {
            Self::Graph(watch) => watch.stop(),
            Self::Document(watch) => watch.stop(),
            Self::Conversation(watch) => watch.stop(),
            Self::Events(stream) => stream.watch.stop(),
        }
    }
    fn cancel(&self) -> WatchEnd {
        match self {
            Self::Graph(watch) => watch.cancel(),
            Self::Document(watch) => watch.cancel(),
            Self::Conversation(watch) => watch.cancel(),
            Self::Events(stream) => stream.watch.cancel(),
        }
    }
    fn value(&self, hub: &Hub) -> JsonValue {
        match self {
            Self::Graph(watch) => hub.revision(watch.value()),
            Self::Document(watch) => hub.native_value(&watch.value()),
            Self::Conversation(watch) => hub.shared_revision(watch.value().revision()),
            Self::Events(stream) => hub.revision(stream.watch.value()),
        }
    }
}
#[derive(Clone)]
struct ObservedWatch {
    watch: ValueWatch,
    activity: tokio::sync::watch::Sender<Activity>,
}
impl GraphBridge {
    pub(super) async fn response_boundary(&self) {
        let watches = self.watches.lock().values().cloned().collect::<Vec<_>>();
        for observed in watches {
            let mut activity = observed.activity.subscribe();
            loop {
                if activity.borrow().active {
                    break;
                }
                tokio::select! { _ = observed.watch.delivery_idle() => break, changed = activity.changed() => if changed.is_err() { break; } }
            }
        }
    }
}
fn end(reason: WatchEnd) -> JsonValue {
    match reason {
        WatchEnd::Stopped => encode(&serde_json::json!({"reason":"stopped"})),
        WatchEnd::Cancelled => encode(&serde_json::json!({"reason":"cancelled"})),
        WatchEnd::Retired => encode(&serde_json::json!({"reason":"retired"})),
        WatchEnd::Ended => encode(&serde_json::json!({"reason":"ended"})),
        WatchEnd::Failed { error } => object([
            ("reason", encode(&"failed")),
            ("error", error_value(&Error::from(error))),
        ]),
        WatchEnd::ListenerError { error } => object([
            ("reason", encode(&"listener_error")),
            ("error", error_value(&Error::from(error))),
        ]),
    }
}
impl Hub {
    fn shared_node(&self, value: &SharedJson) -> JsonValue {
        if let Some(value) = value.as_scalar() {
            return object([("scalar", value.clone())]);
        }
        let id = {
            let mut shared = self.graph.shared.lock();
            if let Some((id, _)) = shared
                .iter()
                .find(|(_, held)| held.upgrade().is_some_and(|held| held.ptr_eq(value)))
            {
                *id
            } else {
                let id = self.next.fetch_add(1, Ordering::Relaxed);
                shared.push((id, value.downgrade()));
                id
            }
        };
        if let Some(items) = value.as_array() {
            object([
                ("id", encode(&id)),
                (
                    "array",
                    JsonValue::Array(
                        items
                            .iter()
                            .map(|item| self.shared_node(item))
                            .collect::<Vec<_>>()
                            .into(),
                    ),
                ),
            ])
        } else {
            let fields: Vec<_> = value
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| {
                    JsonValue::Array(
                        vec![JsonValue::String(key.clone()), self.shared_node(value)].into(),
                    )
                })
                .collect();
            object([
                ("id", encode(&id)),
                ("object", JsonValue::Array(fields.into())),
            ])
        }
    }
    pub(super) fn shared_revision(&self, value: &SharedJson) -> JsonValue {
        object([("shared", self.shared_node(value))])
    }
    pub(super) fn revision<T: Serialize + Send + Sync + 'static>(
        &self,
        value: Arc<T>,
    ) -> JsonValue {
        let mut revisions = self.graph.revisions.lock();
        let erased: Arc<dyn Any + Send + Sync> = value.clone();
        let revision = if let Some((id, _)) = revisions.iter().find(|(_, held)| {
            held.upgrade()
                .is_some_and(|held| Arc::ptr_eq(&held, &erased))
        }) {
            *id
        } else {
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            revisions.push((id, Arc::downgrade(&erased)));
            id
        };
        object([
            ("revision", encode(&revision)),
            ("value", encode(value.as_ref())),
        ])
    }
    pub(super) fn register_watch(&self, watch: ValueWatch) -> JsonValue {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let value = watch.value(self);
        let document = matches!(watch, ValueWatch::Document(_));
        let (activity, _) = tokio::sync::watch::channel(Activity::default());
        self.graph
            .watches
            .lock()
            .insert(id, ObservedWatch { watch, activity });
        object([
            ("watch", encode(&id)),
            ("snapshot", value),
            ("document", encode(&document)),
        ])
    }
    fn start_watch<T: Serialize + Send + Sync + 'static>(
        self: &Arc<Self>,
        watch: Watch<T>,
        document: bool,
        id: u64,
        callback: u64,
        activity: tokio::sync::watch::Sender<Activity>,
        serialize: impl Fn(&Hub, Arc<T>) -> JsonValue + Send + Sync + 'static,
    ) -> Result<()> {
        let hub = self.clone();
        watch
            .start_with_error(move |frame| {
                if document {
                    hub.documents.watch_frames.fetch_add(1, Ordering::Relaxed);
                } else {
                    hub.graph.frames.fetch_add(1, Ordering::Relaxed);
                }
                let serial = activity.borrow().serial + 1;
                let args = object([
                    ("watch", encode(&id)),
                    ("serial", encode(&serial)),
                    ("document", encode(&document)),
                    ("callback", encode(&callback)),
                    ("snapshot", serialize(&hub, frame.value)),
                    ("ops", hub.native_ops(&frame.ops)),
                ]);
                let call = hub.call_context("graphFrame", args, &frame.context);
                activity.send_replace(Activity {
                    serial,
                    active: true,
                });
                let activity = activity.clone();
                async move {
                    let result = call.await.map(|_| ()).map_err(pi_chord::WatchError::from);
                    activity.send_modify(|activity| activity.active = false);
                    result
                }
            })
            .map_err(Error::message)
    }
    pub(super) fn graph_request(
        self: &Arc<Self>,
        method: &str,
        request: JsonValue,
        context: Context,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        if method == "graph.watch" {
            self.graph.reads.fetch_add(1, Ordering::Relaxed);
            let watch = self.scheduler(&request)?.watch_task_graph(&context);
            let hub = self.clone();
            return Ok(Box::pin(async move {
                Ok(Some(hub.register_watch(ValueWatch::Graph(watch.await?))))
            }));
        }
        let id: u64 = field(&request, "watch")?;
        let observed = self
            .graph
            .watches
            .lock()
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::message("Unknown native watch"))?;
        let watch = observed.watch;
        if method == "graph.turn" {
            let serial: u64 = field(&request, "serial")?;
            return Ok(Box::pin(async move {
                let mut activity = observed.activity.subscribe();
                loop {
                    if activity.borrow().serial > serial {
                        break;
                    }
                    tokio::select! { _ = watch.delivery_idle() => break, changed = activity.changed() => if changed.is_err() { break; } }
                }
                Ok(None)
            }));
        }
        let result = match method {
            "graph.value" => Some(watch.value(self)),
            "graph.start" => {
                let callback = field(&request, "callback")?;
                match watch {
                    ValueWatch::Graph(watch) => self.start_watch(
                        watch,
                        false,
                        id,
                        callback,
                        observed.activity,
                        Hub::revision,
                    )?,
                    ValueWatch::Document(watch) => self.start_watch(
                        watch,
                        true,
                        id,
                        callback,
                        observed.activity,
                        |hub, value| hub.native_value(&value),
                    )?,
                    ValueWatch::Conversation(watch) => self.start_watch(
                        watch,
                        false,
                        id,
                        callback,
                        observed.activity,
                        |hub, value| hub.shared_revision(value.revision()),
                    )?,
                    ValueWatch::Events(stream) => self.start_watch(
                        stream.watch,
                        false,
                        id,
                        callback,
                        observed.activity,
                        |hub, value| {
                            hub.views.event_frames.fetch_add(1, Ordering::Relaxed);
                            hub.revision(value)
                        },
                    )?,
                }
                None
            }
            "graph.stop" => Some(end(watch.stop())),
            "graph.cancel" => Some(end(watch.cancel())),
            "graph.closed" => {
                return Ok(Box::pin(async move { Ok(Some(end(watch.closed().await))) }));
            }
            other => return Err(Error::message(format!("Unknown watch method {other}"))),
        };
        Ok(Box::pin(async move { Ok(result) }))
    }
}
