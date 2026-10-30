//! Hosted model collections preserve original objects until the native durable writer.
use super::*;
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use pi_durable::harness::{models::*, types::ModelRef};
use std::sync::Weak;
#[derive(Default)]
pub(super) struct ModelBridge {
    streams: Mutex<HashMap<u64, tokio::sync::mpsc::UnboundedSender<AssistantValueRef>>>,
    values: Mutex<HashMap<u64, Weak<AssistantValue>>>,
    pub requests: AtomicU64,
    pub polls: AtomicU64,
    pub cancellations: AtomicU64,
    pub hook_calls: AtomicU64,
    pub completions: AtomicU64,
    pub compaction_hooks: AtomicU64,
}
pub(super) struct HostAssistant {
    pub hub: Weak<Hub>,
    pub id: u64,
}
impl HostAssistant {
    fn hub(&self) -> Result<Arc<Hub>> {
        self.hub
            .upgrade()
            .ok_or_else(|| Error::message("Model value host closed"))
    }
}
impl HostedAssistantValue for HostAssistant {
    fn field(&self, name: &str) -> Result<Option<JsonValue>> {
        let value = self.hub()?.sync(
            "models.field",
            object([("message", encode(&self.id)), ("name", encode(&name))]),
        )?;
        Ok(if value["present"].as_bool() == Some(true) {
            Some(value["value"].clone())
        } else {
            None
        })
    }
    fn set_field(&self, name: JsonString, value: Option<JsonValue>) -> Result<()> {
        self.hub()?.sync(
            "models.setField",
            object([
                ("message", encode(&self.id)),
                ("name", encode(&name)),
                ("present", encode(&value.is_some())),
                ("value", value.unwrap_or(JsonValue::Null)),
            ]),
        )?;
        Ok(())
    }
    fn copy_json(&self) -> Result<JsonObject> {
        decode(&self.hub()?.sync("models.copy", encode(&self.id))?)
    }
    fn has_content(&self) -> Result<bool> {
        decode(&self.hub()?.sync("models.hasContent", encode(&self.id))?)
    }
    fn tool_calls(&self) -> Result<Vec<JsonValue>> {
        decode(&self.hub()?.sync("models.toolCalls", encode(&self.id))?)
    }
    fn has_tool_calls(&self) -> Result<bool> {
        decode(&self.hub()?.sync("models.hasToolCalls", encode(&self.id))?)
    }
    fn text_blocks(&self) -> Result<Vec<JsonString>> {
        decode(&self.hub()?.sync("models.textBlocks", encode(&self.id))?)
    }
}
struct HostModel(u64);
struct HostCollection {
    hub: Weak<Hub>,
    id: u64,
}
impl HostCollection {
    fn hub(&self) -> Result<Arc<Hub>> {
        self.hub
            .upgrade()
            .ok_or_else(|| Error::message("Model collection host closed"))
    }
    fn invoke(
        &self,
        method: &'static str,
        model: ResolvedModel,
        request: JsonValue,
        options: JsonObject,
        context: Context,
        stream: Option<u64>,
    ) -> StorageFuture<AssistantValueRef> {
        let operation = (|| {
            let hub = self.hub()?;
            let model = model.value::<HostModel>()?.0;
            let id = stream.unwrap_or_else(|| hub.next.fetch_add(1, Ordering::Relaxed));
            let signal = context.abort_signal().cloned();
            let operation = hub.call_context(
                "models.request",
                object([
                    ("collection", encode(&self.id)),
                    ("stream", encode(&id)),
                    ("method", encode(&method)),
                    ("model", encode(&model)),
                    ("request", request),
                    ("options", encode(&options)),
                    (
                        "aborted",
                        encode(&signal.as_ref().is_some_and(|signal| signal.aborted())),
                    ),
                ]),
                &context,
            );
            Ok::<_, Error>((hub, id, signal, operation))
        })();
        Box::pin(async move {
            let (hub, id, signal, operation) = operation?;
            tokio::pin!(operation);
            let result = if let Some(signal) = signal {
                tokio::select! {result=&mut operation=>result,_=signal.cancelled()=>{hub.emit(object([("type",encode(&"models.signal")),("stream",encode(&id))]));operation.await}}
            } else {
                operation.await
            };
            result
                .and_then(|value| value.ok_or_else(|| Error::message("Model returned undefined")))
                .and_then(|value| hub.host_message(decode(&value)?))
        })
    }
}
struct HostStream {
    receiver: tokio::sync::mpsc::UnboundedReceiver<AssistantValueRef>,
    result: Shared<BoxFuture<'static, Result<AssistantValueRef>>>,
}
impl ModelStream for HostStream {
    fn next(&mut self) -> ModelFuture<'_, Option<AssistantValueRef>> {
        Box::pin(async move { Ok(self.receiver.recv().await) })
    }
    fn result(&mut self) -> ModelFuture<'_, AssistantValueRef> {
        Box::pin(self.result.clone())
    }
}
impl ModelCollection for HostCollection {
    fn codemode_models(&self) -> Option<Arc<dyn pi_durable::codemode::models::CodemodeModels>> {
        Some(Arc::new(super::codemode_bridge::HostCodemodeModels {
            hub: self.hub.clone(),
            id: self.id,
        }))
    }
    fn get_model(&self, reference: &ModelRef) -> Result<Option<ResolvedModel>> {
        let value = self.hub()?.sync(
            "models.get",
            object([
                ("collection", encode(&self.id)),
                ("reference", encode(reference)),
            ]),
        )?;
        if value.is_null() {
            return Ok(None);
        }
        let id: u64 = decode(&value)?;
        let host = self.hub.clone();
        let image_host = self.hub.clone();
        Ok(Some(
            ResolvedModel::new(reference.clone(), 0.0, HostModel(id))
                .with_metadata(Arc::new(move |property| {
                    let hub = host
                        .upgrade()
                        .ok_or_else(|| Error::message("Model host closed"))?;
                    let value = hub.sync(
                        "models.metadata",
                        object([
                            ("model", encode(&id)),
                            ("property", encode(&property.name())),
                        ]),
                    )?;
                    match value.to_utf8().as_deref() {
                        Some("Infinity") => Ok(f64::INFINITY),
                        Some("-Infinity") => Ok(f64::NEG_INFINITY),
                        Some("NaN") => Ok(f64::NAN),
                        _ => decode(&value),
                    }
                }))
                .with_image_input(Arc::new(move || {
                    let hub = image_host
                        .upgrade()
                        .ok_or_else(|| Error::message("Model host closed"))?;
                    let value = hub.sync("models.imageInput", encode(&id))?;
                    let optional = |key: &str| -> Result<Option<f64>> {
                        if value[key].is_null() {
                            return Ok(None);
                        }
                        value[key]
                            .to_utf8()
                            .map_or_else(
                                || decode(&value[key]),
                                |s| {
                                    s.parse().map_err(|e: std::num::ParseFloatError| {
                                        Error::message(e.to_string())
                                    })
                                },
                            )
                            .map(Some)
                    };
                    Ok(ModelImageInput {
                        vision: field(&value, "vision")?,
                        max_width: optional("maxWidth")?,
                        max_height: optional("maxHeight")?,
                        max_bytes: optional("maxBytes")?,
                    })
                })),
        ))
    }
    fn stream_simple(
        &self,
        model: ResolvedModel,
        messages: Vec<Arc<JsonValue>>,
        options: JsonObject,
        context: Context,
    ) -> StorageFuture<Box<dyn ModelStream>> {
        let operation = (|| {
            let hub = self.hub()?;
            hub.models.requests.fetch_add(1, Ordering::Relaxed);
            let id = hub.next.fetch_add(1, Ordering::Relaxed);
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            hub.models.streams.lock().insert(id, sender);
            let operation = self.invoke(
                "streamSimple",
                model,
                object([("messages", encode(&messages))]),
                options,
                context,
                Some(id),
            );
            Ok::<_, Error>((hub, id, receiver, operation))
        })();
        Box::pin(async move {
            let (hub, id, receiver, operation) = operation?;
            let (send, result) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let result = operation.await;
                hub.models.streams.lock().remove(&id);
                let _ = send.send(result);
            });
            let result = async move {
                result
                    .await
                    .map_err(|error| Error::message(error.to_string()))?
            }
            .boxed()
            .shared();
            Ok(Box::new(HostStream { receiver, result }) as Box<dyn ModelStream>)
        })
    }
    fn fetch_deferred(
        &self,
        model: ResolvedModel,
        handle: JsonObject,
        context: Context,
    ) -> StorageFuture<AssistantValueRef> {
        if let Ok(hub) = self.hub() {
            hub.models.polls.fetch_add(1, Ordering::Relaxed);
        }
        self.invoke(
            "fetchDeferred",
            model,
            handle.into_value(),
            JsonObject::default(),
            context,
            None,
        )
    }
    fn complete_simple(
        &self,
        model: ResolvedModel,
        messages: Vec<Arc<JsonValue>>,
        options: JsonObject,
        context: Context,
    ) -> StorageFuture<AssistantValueRef> {
        if let Ok(hub) = self.hub() {
            hub.models.completions.fetch_add(1, Ordering::Relaxed);
        }
        self.invoke(
            "completeSimple",
            model,
            object([("messages", encode(&messages))]),
            options,
            context,
            None,
        )
    }
    fn cancel_deferred(
        &self,
        model: ResolvedModel,
        handle: JsonObject,
        context: Context,
    ) -> StorageFuture<()> {
        let operation = (|| {
            let hub = self.hub()?;
            hub.models.cancellations.fetch_add(1, Ordering::Relaxed);
            let id = hub.next.fetch_add(1, Ordering::Relaxed);
            let signal = context.abort_signal().cloned();
            let operation = hub.call_context(
                "models.cancel",
                object([
                    ("collection", encode(&self.id)),
                    ("stream", encode(&id)),
                    ("model", encode(&model.value::<HostModel>()?.0)),
                    ("handle", encode(&handle)),
                    (
                        "aborted",
                        encode(&signal.as_ref().is_some_and(|signal| signal.aborted())),
                    ),
                ]),
                &context,
            );
            Ok::<_, Error>((hub, id, signal, operation))
        })();
        Box::pin(async move {
            let (hub, id, signal, operation) = operation?;
            tokio::pin!(operation);
            if let Some(signal) = signal {
                tokio::select! {result=&mut operation=>{result?;},_=signal.cancelled()=>{hub.emit(object([("type",encode(&"models.signal")),("stream",encode(&id))]));operation.await?;}}
            } else {
                operation.await?;
            }
            Ok(())
        })
    }
}
impl Hub {
    pub(super) fn host_collection(
        self: &Arc<Self>,
        harness: u64,
    ) -> Result<Arc<dyn ModelCollection>> {
        let id = decode(&self.sync("models.collection", encode(&harness))?)?;
        Ok(Arc::new(HostCollection {
            hub: Arc::downgrade(self),
            id,
        }))
    }
    pub(super) fn host_message(self: &Arc<Self>, id: u64) -> Result<AssistantValueRef> {
        let mut values = self.models.values.lock();
        if let Some(value) = values.get(&id).and_then(Weak::upgrade) {
            return Ok(value);
        }
        let value = AssistantValue::hosted(Arc::new(HostAssistant {
            hub: Arc::downgrade(self),
            id,
        }));
        values.insert(id, Arc::downgrade(&value));
        Ok(value)
    }
    pub(super) fn message_handle(self: &Arc<Self>, value: &AssistantValueRef) -> Result<u64> {
        if let Some(value) = value.host::<HostAssistant>() {
            Ok(value.id)
        } else {
            decode(&self.sync("models.capture", encode(&value.copy_json()?))?)
        }
    }
    pub(super) fn model_event(self: &Arc<Self>, value: &JsonValue) -> Result<()> {
        let stream = field::<u64>(value, "stream")?;
        let partial = self.host_message(field(value, "message")?)?;
        if let Some(sender) = self.models.streams.lock().get(&stream) {
            let _ = sender.send(partial);
        }
        Ok(())
    }
}
