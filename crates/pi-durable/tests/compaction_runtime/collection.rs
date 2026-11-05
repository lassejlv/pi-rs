use super::*;
use pi_durable::harness::{models::*, types::ModelRef, usage::USAGE_DOC};

#[derive(Clone, Copy)]
enum Response {
    Summary,
    BadUsage,
    Throw,
    ToolCall,
    BadLimit,
}
struct HostedMessage {
    response: Response,
    copies: Arc<AtomicUsize>,
}
impl HostedAssistantValue for HostedMessage {
    fn field(&self, name: &str) -> Result<Option<JsonValue>> {
        if name == "errorMessage" {
            return Err(Error::message("unused error getter must not be read"));
        }
        Ok(match name {
            "provider" => Some(json("\"compaction-test\"")),
            "model" => Some(json("\"fixture\"")),
            "stopReason" => Some(json(if matches!(self.response, Response::ToolCall) {
                "\"toolUse\""
            } else {
                "\"stop\""
            })),
            "usage" if matches!(self.response, Response::BadUsage) => {
                return Err(Error::message("non-JSON usage function"));
            }
            "usage" => Some(json(
                r#"{"input":17,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":18}"#,
            )),
            "content" if matches!(self.response, Response::ToolCall) => {
                return Err(Error::message("non-JSON tool arguments"));
            }
            "content" => Some(json(r#"[{"type":"text","text":"\ufeff \ud800 \ufeff"}]"#)),
            _ => None,
        })
    }
    fn set_field(&self, _: JsonString, _: Option<JsonValue>) -> Result<()> {
        Err(Error::message("unexpected mutation"))
    }
    fn copy_json(&self) -> Result<JsonObject> {
        self.copies.fetch_add(1, Ordering::SeqCst);
        Err(Error::message("non-JSON unused provider property"))
    }
    fn has_tool_calls(&self) -> Result<bool> {
        Ok(matches!(self.response, Response::ToolCall))
    }
}
struct Collection {
    response: Response,
    lookups: AtomicUsize,
    completions: AtomicUsize,
    copies: Arc<AtomicUsize>,
    instructions: JsonString,
    metadata_reads: Arc<AtomicUsize>,
}
impl ModelCollection for Collection {
    fn get_model(&self, reference: &ModelRef) -> Result<Option<ResolvedModel>> {
        let lookup = self.lookups.fetch_add(1, Ordering::SeqCst) + 1;
        let reads = self.metadata_reads.clone();
        let response = self.response;
        Ok(Some(
            ResolvedModel::new(reference.clone(), 0.0, lookup).with_metadata(Arc::new(
                move |property| match property {
                    ModelProperty::ContextWindow => {
                        Err(Error::message("compaction must not read contextWindow"))
                    }
                    ModelProperty::MaxTokens if lookup == 1 => {
                        Ok(if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                            6.0
                        } else if matches!(response, Response::BadLimit) {
                            f64::NAN
                        } else {
                            2.5
                        })
                    }
                    ModelProperty::MaxTokens => {
                        Err(Error::message("summarize must use its pinned maxTokens"))
                    }
                },
            )),
        ))
    }
    fn stream_simple(
        &self,
        _: ResolvedModel,
        _: Vec<Arc<JsonValue>>,
        _: JsonObject,
        _: Context,
    ) -> storage::StorageFuture<Box<dyn ModelStream>> {
        Box::pin(async { Err(Error::message("completeSimple override was bypassed")) })
    }
    fn complete_simple(
        &self,
        model: ResolvedModel,
        messages: Vec<Arc<JsonValue>>,
        options: JsonObject,
        context: Context,
    ) -> storage::StorageFuture<AssistantValueRef> {
        self.completions.fetch_add(1, Ordering::SeqCst);
        assert_eq!(*model.value::<usize>().unwrap(), 2);
        assert_eq!(options.value()["maxTokens"].as_f64(), Some(2.5));
        assert!(options.value()["cacheRetention"].string_equals("none"));
        assert!(!options.contains_key(&JsonString::from("deferred")));
        assert!(options.value()["sessionId"].as_string().is_some());
        assert!(context.abort_signal().is_some());
        assert_eq!(messages.len(), 2);
        let prompt = messages[1]["content"][0]["text"].as_string().unwrap();
        assert!(prompt.units().ends_with(self.instructions.units()));
        let response = self.response;
        let copies = self.copies.clone();
        Box::pin(async move {
            if matches!(response, Response::Throw) {
                Err(Error::message("original completeSimple exception"))
            } else {
                Ok(AssistantValue::hosted(Arc::new(HostedMessage {
                    response,
                    copies,
                })))
            }
        })
    }
    fn fetch_deferred(
        &self,
        _: ResolvedModel,
        _: JsonObject,
        _: Context,
    ) -> storage::StorageFuture<AssistantValueRef> {
        Box::pin(async { Err(Error::message("unexpected deferred request")) })
    }
    fn cancel_deferred(
        &self,
        _: ResolvedModel,
        _: JsonObject,
        _: Context,
    ) -> storage::StorageFuture<()> {
        Box::pin(async { Err(Error::message("unexpected deferred cancellation")) })
    }
}

#[tokio::test]
async fn a_non_finite_second_limit_is_not_silently_replaced_with_the_reserve() {
    for retention in [600_000.0, 0.0] {
        let collection = Arc::new(Collection {
            response: Response::BadLimit,
            lookups: AtomicUsize::new(0),
            completions: AtomicUsize::new(0),
            copies: Arc::new(AtomicUsize::new(0)),
            instructions: "focus".into(),
            metadata_reads: Arc::new(AtomicUsize::new(0)),
        });
        let current = collection.clone();
        let (scheduler, conversation, _) = open_with_collection(
            retention,
            Arc::new(Mutex::new(Models::default())),
            Arc::new(Mutex::new(settings())),
            vec![],
            true,
            Some(Arc::new(move || Ok(current.clone()))),
        )
        .await;
        seed(&scheduler, conversation).await;
        let id = compact_conversation(
            &scheduler,
            conversation,
            Some("focus".into()),
            &Context::default(),
        )
        .await
        .unwrap();
        assert!(matches!(
            settled(&scheduler, id.untyped()).await.state,
            TaskState::Terminal {
                outcome: TaskOutcome::Faulted { .. }
            }
        ));
        assert_eq!(collection.metadata_reads.load(Ordering::SeqCst), 2);
        assert_eq!(collection.completions.load(Ordering::SeqCst), 0);
        let usage = scheduler
            .session()
            .snapshot(&USAGE_DOC, conversation, &Context::default())
            .await
            .unwrap()
            .unwrap();
        assert!(usage.models.is_empty());
        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
    }
}
async fn run(
    retention: f64,
    response: Response,
) -> (
    TaskScheduler,
    ConversationId,
    Arc<MemoryStorage>,
    TaskRecord,
    Arc<Collection>,
) {
    let instructions = JsonString::from_utf16(vec![0xd801]);
    let collection = Arc::new(Collection {
        response,
        lookups: AtomicUsize::new(0),
        completions: AtomicUsize::new(0),
        copies: Arc::new(AtomicUsize::new(0)),
        instructions: instructions.clone(),
        metadata_reads: Arc::new(AtomicUsize::new(0)),
    });
    let current = collection.clone();
    let (scheduler, conversation, storage) = open_with_collection(
        retention,
        Arc::new(Mutex::new(Models::default())),
        Arc::new(Mutex::new(settings())),
        vec![],
        true,
        Some(Arc::new(move || Ok(current.clone()))),
    )
    .await;
    seed(&scheduler, conversation).await;
    let id = compact_conversation(
        &scheduler,
        conversation,
        Some(instructions),
        &Context::default(),
    )
    .await
    .unwrap();
    // compact() enables scheduling; no explicit resume or wait-for-task is needed.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let task = storage
                .task(id.untyped(), &Context::default())
                .await
                .unwrap()
                .unwrap();
            if matches!(task.state, TaskState::Terminal { .. }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let record = settled(&scheduler, id.untyped()).await;
    assert_eq!(collection.lookups.load(Ordering::SeqCst), 2);
    assert_eq!(collection.completions.load(Ordering::SeqCst), 1);
    assert_eq!(collection.metadata_reads.load(Ordering::SeqCst), 2);
    assert_eq!(collection.copies.load(Ordering::SeqCst), 0);
    (scheduler, conversation, storage, record, collection)
}

#[tokio::test]
async fn hosted_summary_preserves_utf16_and_ignores_unused_non_json_properties() {
    for retention in [600_000.0, 0.0] {
        let (scheduler, conversation, storage, record, _) = run(retention, Response::Summary).await;
        let result = completed(&record);
        let submission = storage
            .submission(result.submission_id.unwrap(), &Context::default())
            .await
            .unwrap()
            .unwrap();
        let SubmissionState::WriteDone { entry } = submission.state else {
            panic!("summary was not placed")
        };
        let entry = storage
            .entry(entry, &Context::default())
            .await
            .unwrap()
            .unwrap();
        let text = entry.entry.model.as_ref().unwrap()[0]["content"][0]["text"]
            .as_string()
            .unwrap();
        let mut expected = JsonString::from(
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\n",
        );
        expected.append(&JsonString::from_utf16(vec![0xd800]));
        expected.append(&"\n</summary>".into());
        assert_eq!(text, &expected);
        let usage = scheduler
            .session()
            .snapshot(&USAGE_DOC, conversation, &Context::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            usage.models[&JsonString::from("compaction-test/fixture")].value()["input"].as_f64(),
            Some(17.0)
        );
        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn collection_and_usage_exceptions_fault_and_clear_status_without_spend_or_summary() {
    for retention in [600_000.0, 0.0] {
        for response in [Response::BadUsage, Response::Throw] {
            let (scheduler, conversation, storage, record, _) = run(retention, response).await;
            let TaskState::Terminal {
                outcome: TaskOutcome::Faulted { error },
            } = record.state
            else {
                panic!("expected original collection fault")
            };
            let expected = match response {
                Response::BadUsage => "non-JSON usage function",
                Response::Throw => "original completeSimple exception",
                _ => unreachable!(),
            };
            assert!(
                error.message.equals_str(expected),
                "unexpected fault: {}",
                error.message
            );
            let usage = scheduler
                .session()
                .snapshot(&USAGE_DOC, conversation, &Context::default())
                .await
                .unwrap()
                .unwrap();
            assert!(usage.models.is_empty());
            let live = scheduler
                .session()
                .snapshot(&LIVE_DOC, conversation, &Context::default())
                .await
                .unwrap()
                .unwrap();
            assert!(live.compactions.is_none());
            let entries = storage
                .scan_entries(
                    &EntryQuery {
                        conversation_id: conversation,
                        min_entry_id: None,
                        max_entry_id: None,
                        order: None,
                    },
                    100,
                    None,
                    &Context::default(),
                )
                .await
                .unwrap();
            assert_eq!(entries.items.len(), 4);
            scheduler
                .session()
                .close(&Context::default())
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn tool_attempt_with_non_json_arguments_fails_as_a_summary_and_records_its_spend() {
    for retention in [600_000.0, 0.0] {
        let (scheduler, conversation, _, record, _) = run(retention, Response::ToolCall).await;
        let TaskState::Terminal {
            outcome: TaskOutcome::Failed { error, .. },
        } = record.state
        else {
            panic!("unexpected outcome")
        };
        assert!(
            error
                .message
                .equals_str("Summarization attempted to call a tool")
        );
        let usage = scheduler
            .session()
            .snapshot(&USAGE_DOC, conversation, &Context::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            usage.models[&JsonString::from("compaction-test/fixture")].value()["input"].as_f64(),
            Some(17.0)
        );
        scheduler
            .session()
            .close(&Context::default())
            .await
            .unwrap();
    }
}
