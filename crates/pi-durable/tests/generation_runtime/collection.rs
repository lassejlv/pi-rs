//! Host values remain opaque until the durable write, including through hooks.
use super::*;
use pi_durable::harness::{models::*, types::ModelRef, usage::USAGE_DOC};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct HostedMessage {
    fields: Mutex<JsonObject>,
    function_property: AtomicBool,
    copies: AtomicUsize,
}
impl HostedMessage {
    fn new() -> Arc<Self> {
        let mut message = faux_text_message("answer");
        message.usage.input = 17;
        Arc::new(Self {
            fields: Mutex::new(
                JsonObject::try_from_value(
                    pi_chord::json::to_value(&pi_ai::Message::Assistant(message)).unwrap(),
                )
                .unwrap(),
            ),
            function_property: AtomicBool::new(true),
            copies: AtomicUsize::new(0),
        })
    }
}
impl HostedAssistantValue for HostedMessage {
    fn field(&self, name: &str) -> Result<Option<JsonValue>> {
        if name == "function" && self.function_property.load(Ordering::SeqCst) {
            return Err(Error::message("non-JSON function"));
        }
        Ok(self.fields.lock().get(&JsonString::from(name)).cloned())
    }
    fn set_field(&self, name: JsonString, value: Option<JsonValue>) -> Result<()> {
        if name.equals_str("function") {
            self.function_property.store(false, Ordering::SeqCst);
        }
        if let Some(value) = value {
            self.fields.lock().insert(name, value);
        } else {
            self.fields.lock().shift_remove(&name);
        }
        Ok(())
    }
    fn copy_json(&self) -> Result<JsonObject> {
        self.copies.fetch_add(1, Ordering::SeqCst);
        if self.function_property.load(Ordering::SeqCst) {
            Err(Error::message("non-JSON function"))
        } else {
            Ok(self.fields.lock().clone())
        }
    }
}
struct Stream {
    partials: VecDeque<AssistantValueRef>,
    final_value: Result<AssistantValueRef>,
    pause: Option<Arc<tokio::sync::Notify>>,
}
impl ModelStream for Stream {
    fn next(&mut self) -> ModelFuture<'_, Option<AssistantValueRef>> {
        Box::pin(async move {
            if let Some(partial) = self.partials.pop_front() {
                return Ok(Some(partial));
            }
            if let Some(pause) = self.pause.take() {
                pause.notified().await;
            }
            Ok(None)
        })
    }
    fn result(&mut self) -> ModelFuture<'_, AssistantValueRef> {
        Box::pin(std::future::ready(self.final_value.clone()))
    }
}
struct Collection {
    stream: Mutex<Option<Stream>>,
    lookups: AtomicUsize,
    requests: AtomicUsize,
    metadata: Option<Metadata>,
}
type Metadata = Arc<dyn Fn(usize, ModelProperty) -> Result<f64> + Send + Sync>;
impl Collection {
    fn new(final_value: Result<AssistantValueRef>) -> Arc<Self> {
        Arc::new(Self {
            stream: Mutex::new(Some(Stream {
                partials: VecDeque::new(),
                final_value,
                pause: None,
            })),
            lookups: AtomicUsize::new(0),
            requests: AtomicUsize::new(0),
            metadata: None,
        })
    }
    fn reader(self: &Arc<Self>) -> ModelCollectionReader {
        let collection = self.clone();
        Arc::new(move || Ok(collection.clone()))
    }
}
impl ModelCollection for Collection {
    fn get_model(&self, reference: &ModelRef) -> Result<Option<ResolvedModel>> {
        let lookup = self.lookups.fetch_add(1, Ordering::SeqCst) + 1;
        let mut model = ResolvedModel::new(reference.clone(), 100_000.0, lookup);
        if let Some(metadata) = self.metadata.clone() {
            model = model.with_metadata(Arc::new(move |property| metadata(lookup, property)));
        }
        Ok(Some(model))
    }
    fn stream_simple(
        &self,
        model: ResolvedModel,
        messages: Vec<Arc<JsonValue>>,
        options: JsonObject,
        context: Context,
    ) -> storage::StorageFuture<Box<dyn ModelStream>> {
        // Preparation and request resolve separately; use the original request model.
        assert_eq!(*model.value::<usize>().unwrap(), 2);
        assert_eq!(messages.last().unwrap()["content"], json("\"hi\""));
        assert!(options.value()["sessionId"].as_string().is_some());
        assert!(context.abort_signal().is_some());
        self.requests.fetch_add(1, Ordering::SeqCst);
        let stream = self
            .stream
            .lock()
            .take()
            .expect("unexpected second request");
        Box::pin(std::future::ready(Ok(
            Box::new(stream) as Box<dyn ModelStream>
        )))
    }
    fn fetch_deferred(
        &self,
        _: ResolvedModel,
        _: JsonObject,
        _: Context,
    ) -> storage::StorageFuture<AssistantValueRef> {
        Box::pin(async { Err(Error::message("unexpected poll")) })
    }
    fn cancel_deferred(
        &self,
        _: ResolvedModel,
        _: JsonObject,
        _: Context,
    ) -> storage::StorageFuture<()> {
        Box::pin(async { Err(Error::message("unexpected cancellation")) })
    }
}

#[tokio::test]
async fn model_metadata_is_read_when_preparing_and_unused_output_limits_are_ignored() {
    for retention in [600_000, 0] {
        let hosted = HostedMessage::new();
        let mut collection =
            Collection::new(Ok(AssistantValue::json(hosted.fields.lock().clone())));
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        Arc::get_mut(&mut collection).unwrap().metadata =
            Some(Arc::new(move |lookup, property| match property {
                ModelProperty::ContextWindow if lookup == 1 => {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(100_000.0)
                }
                ModelProperty::ContextWindow => {
                    Err(Error::message("request must not read contextWindow"))
                }
                ModelProperty::MaxTokens => {
                    Err(Error::message("generation must not read maxTokens"))
                }
            }));
        let opened = open_with_collection(retention, true, None, Some(collection.reader())).await;
        assert!(
            pi_chord::json::to_value(&submit(&opened, "hi").await).unwrap()["status"]
                .string_equals("done")
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(collection.lookups.load(Ordering::SeqCst), 2);
        assert_eq!(collection.requests.load(Ordering::SeqCst), 1);
        close(opened).await;
    }
}

#[tokio::test]
async fn a_used_metadata_getter_exception_faults_before_any_provider_request() {
    for retention in [600_000, 0] {
        let mut collection = Collection::new(Err(Error::message("request must not start")));
        Arc::get_mut(&mut collection).unwrap().metadata = Some(Arc::new(|_, _| {
            Err(Error::message("context getter failed"))
        }));
        let opened = open_with_collection(retention, true, None, Some(collection.reader())).await;
        let result = pi_chord::json::to_value(&submit(&opened, "hi").await).unwrap();
        assert!(result["reason"].string_equals("faulted"));
        assert_eq!(result["detail"], json("\"context getter failed\""));
        assert_eq!(collection.lookups.load(Ordering::SeqCst), 1);
        assert_eq!(collection.requests.load(Ordering::SeqCst), 0);
        assert_no_usage(&opened).await;
        close(opened).await;
    }
}
async fn assert_no_usage(opened: &Opened) {
    let usage = opened
        .scheduler
        .session()
        .snapshot(&USAGE_DOC, opened.root, &Context::default())
        .await
        .unwrap()
        .unwrap();
    assert!(
        usage.models.is_empty(),
        "a rejected entry must roll back its usage"
    );
}

#[tokio::test]
async fn non_json_final_faults_at_the_writer_and_rolls_back_usage() {
    for retention in [600_000, 0] {
        let hosted = HostedMessage::new();
        let collection = Collection::new(Ok(AssistantValue::hosted(hosted.clone())));
        let reads = Arc::new(AtomicUsize::new(0));
        let accessed = reads.clone();
        let current = collection.clone();
        let opened = open_with_collection(
            retention,
            true,
            None,
            Some(Arc::new(move || {
                accessed.fetch_add(1, Ordering::SeqCst);
                Ok(current.clone())
            })),
        )
        .await;
        let result = pi_chord::json::to_value(&submit(&opened, "hi").await).unwrap();
        assert!(result["reason"].string_equals("faulted"));
        assert!(
            result["detail"]
                .to_utf8()
                .unwrap()
                .contains("non-JSON function")
        );
        assert_eq!(hosted.copies.load(Ordering::SeqCst), 1);
        assert_eq!(reads.load(Ordering::SeqCst), 3);
        assert_eq!(collection.lookups.load(Ordering::SeqCst), 2);
        assert_eq!(collection.requests.load(Ordering::SeqCst), 1);
        assert_eq!(entries(&opened).await.len(), 1);
        assert_no_usage(&opened).await;
        close(opened).await;
    }
}

#[tokio::test]
async fn response_and_yield_hooks_can_repair_the_original_hosted_object_before_copy() {
    for retention in [600_000, 0] {
        let hosted = HostedMessage::new();
        let value = AssistantValue::hosted(hosted.clone());
        let collection = Collection::new(Ok(value.clone()));
        let captured = value.clone();
        let yielded = value.clone();
        let mut extension = Extension::new("repair");
        extension.hooks.push(HookRegistration::new(
            "pi.generation",
            Arc::new(GenerationHooks {
                after_response: Some(Arc::new(move |message, _, _| {
                    assert!(Arc::ptr_eq(&message, &captured));
                    assert_eq!(
                        message
                            .host::<HostedMessage>()
                            .unwrap()
                            .copies
                            .load(Ordering::SeqCst),
                        0
                    );
                    message.set_field("observed", Some(json("true"))).unwrap();
                    Box::pin(async { Ok(()) })
                })),
                on_yield: Some(Arc::new(move |message, _, _| {
                    assert!(Arc::ptr_eq(&message, &yielded));
                    assert_eq!(message.field("observed").unwrap(), Some(json("true")));
                    assert_eq!(
                        message
                            .host::<HostedMessage>()
                            .unwrap()
                            .copies
                            .load(Ordering::SeqCst),
                        0
                    );
                    message.set_field("function", None).unwrap();
                    Box::pin(async { Ok(None) })
                })),
                ..Default::default()
            }),
        ));
        let opened =
            open_with_collection(retention, true, Some(extension), Some(collection.reader())).await;
        assert!(
            pi_chord::json::to_value(&submit(&opened, "hi").await).unwrap()["status"]
                .string_equals("done")
        );
        assert_eq!(hosted.copies.load(Ordering::SeqCst), 1);
        assert_eq!(
            entries(&opened).await[1].model.as_ref().unwrap()[0]["observed"],
            json("true")
        );
        close(opened).await;
    }
}

#[tokio::test]
async fn thrown_collection_errors_fault_instead_of_becoming_provider_error_messages() {
    for retention in [600_000, 0] {
        let collection = Collection::new(Err(Error::message("original host failure")));
        let opened = open_with_collection(retention, true, None, Some(collection.reader())).await;
        let result = pi_chord::json::to_value(&submit(&opened, "hi").await).unwrap();
        assert!(result["reason"].string_equals("faulted"));
        assert_eq!(result["detail"], json("\"original host failure\""));
        assert_eq!(collection.requests.load(Ordering::SeqCst), 1);
        assert_eq!(entries(&opened).await.len(), 1);
        assert_no_usage(&opened).await;
        close(opened).await;
    }
}

#[tokio::test]
async fn invalid_partial_is_reported_and_a_valid_final_can_still_settle() {
    for retention in [600_000, 0] {
        let hosted = HostedMessage::new();
        let final_value = AssistantValue::json(hosted.fields.lock().clone());
        let collection = Collection::new(Ok(final_value));
        let pause = Arc::new(tokio::sync::Notify::new());
        collection
            .stream
            .lock()
            .as_mut()
            .unwrap()
            .partials
            .push_back(AssistantValue::hosted(hosted.clone()));
        collection.stream.lock().as_mut().unwrap().pause = Some(pause.clone());
        let opened = open_with_collection(retention, true, None, Some(collection.reader())).await;
        opened.settings.lock().progress.partial_interval_ms = 0.0;
        let reports = opened.reports.clone();
        let release = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(3), async {
                while reports.lock().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            pause.notify_one();
        });
        assert!(
            pi_chord::json::to_value(&submit(&opened, "hi").await).unwrap()["status"]
                .string_equals("done")
        );
        release.await.unwrap();
        assert_eq!(hosted.copies.load(Ordering::SeqCst), 1);
        assert_eq!(opened.reports.lock().len(), 1);
        assert_eq!(opened.reports.lock()[0].to_string(), "non-JSON function");
        assert_eq!(entries(&opened).await.len(), 2);
        close(opened).await;
    }
}
