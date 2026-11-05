//! Native generation binds original section renderers through the task read API.
use super::*;
use pi_durable::harness::{
    declaration::{DeclaredTool, ToolDeclaration},
    generation::*,
    prompt::PromptSection,
};
use registry_bridge::{SectionHandle, ToolHandle};
use std::sync::Weak;

pub(super) struct HostGenerationHooks {
    pub hub: Weak<Hub>,
    pub id: u64,
}
impl HostGenerationHooks {
    fn member(&self, name: &str) -> Result<(Arc<Hub>, Option<u64>)> {
        let hub = self
            .hub
            .upgrade()
            .ok_or_else(|| Error::message("Host closed"))?;
        let callback = decode(&hub.sync(
            "hookMember",
            object([("handlers", encode(&self.id)), ("name", encode(&name))]),
        )?)?;
        Ok((hub, callback))
    }
}
impl GenerationHookProvider for HostGenerationHooks {
    fn before_request(&self) -> Result<Option<BeforeRequest>> {
        let (hub, callback) = self.member("beforeRequest")?;
        Ok(callback.map(|callback| {
            Arc::new(
                move |messages: Vec<Arc<JsonValue>>,
                      runtime: GenerationRuntime,
                      context: Context|
                      -> StorageFuture<Option<Vec<Arc<JsonValue>>>> {
                    let mut args = hub.generation_hook_request(callback, &runtime, &context);
                    args.as_object_mut()
                        .unwrap()
                        .insert("messages".into(), encode(&messages));
                    let operation = hub.call_context("generation.beforeRequest", args, &context);
                    Box::pin(async move {
                        operation
                            .await?
                            .map(|value| field(&value, "messages"))
                            .transpose()
                    })
                },
            ) as BeforeRequest
        }))
    }
    fn after_response(&self) -> Result<Option<AfterResponse>> {
        let (hub, callback) = self.member("afterResponse")?;
        Ok(callback.map(|callback| {
            Arc::new(
                move |message: GenerationMessage,
                      runtime: GenerationRuntime,
                      context: Context|
                      -> StorageFuture<()> {
                    let args =
                        hub.generation_message_hook_request(callback, &message, &runtime, &context);
                    let hub = hub.clone();
                    Box::pin(async move {
                        hub.call_context("generation.afterResponse", args?, &context)
                            .await?;
                        Ok(())
                    })
                },
            ) as AfterResponse
        }))
    }
    fn on_yield(&self) -> Result<Option<OnYield>> {
        let (hub, callback) = self.member("onYield")?;
        Ok(callback.map(|callback| Arc::new(move |message: GenerationMessage, runtime: GenerationRuntime, context: Context| -> StorageFuture<Option<pi_durable::harness::types::UserInput>> {
            let args = hub.generation_message_hook_request(callback, &message, &runtime, &context);
            let hub = hub.clone();
            Box::pin(async move { hub.call_context("generation.onYield", args?, &context).await?.map(|value| field(&value, "continue")).transpose() })
        }) as OnYield))
    }
    fn after_tools(&self) -> Result<Option<AfterTools>> {
        let (hub, callback) = self.member("afterTools")?;
        Ok(callback.map(|callback| {
            Arc::new(
                move |assistant: EntryId,
                      results: Vec<EntryId>,
                      runtime: GenerationRuntime,
                      context: Context|
                      -> StorageFuture<()> {
                    let mut args = hub.generation_hook_request(callback, &runtime, &context);
                    args.as_object_mut().unwrap().extend([
                        ("assistant".into(), encode(&assistant)),
                        ("results".into(), encode(&results)),
                    ]);
                    let operation = hub.call_context("generation.afterTools", args, &context);
                    Box::pin(async move {
                        operation.await?;
                        Ok(())
                    })
                },
            ) as AfterTools
        }))
    }
}
impl Hub {
    fn generation_hook_request(
        self: &Arc<Self>,
        callback: u64,
        runtime: &GenerationRuntime,
        context: &Context,
    ) -> JsonValue {
        self.models.hook_calls.fetch_add(1, Ordering::Relaxed);
        object([
            ("callback", encode(&callback)),
            (
                "runtime",
                encode(&self.bind_tool_runtime(&runtime.erased(), context)),
            ),
            ("taskId", encode(&runtime.task_id())),
            ("conversationId", encode(&runtime.conversation_id())),
            (
                "harness",
                encode(&context.value(&HARNESS_CONTEXT).map(|id| *id)),
            ),
        ])
    }
    fn generation_message_hook_request(
        self: &Arc<Self>,
        callback: u64,
        message: &GenerationMessage,
        runtime: &GenerationRuntime,
        context: &Context,
    ) -> Result<JsonValue> {
        let mut args = self.generation_hook_request(callback, runtime, context);
        args.as_object_mut()
            .unwrap()
            .insert("message".into(), encode(&self.message_handle(message)?));
        Ok(args)
    }
}

impl DeclaredTool for ToolHandle {
    fn declaration(&self) -> ToolDeclaration {
        self.executable
            .as_ref()
            .expect("native generation requires an executable tool registration")
            .declaration()
    }
}
impl Hub {
    pub(super) fn init_conversation(
        self: &Arc<Self>,
        callback: Option<u64>,
        tx: Transaction,
        conversation: ConversationId,
    ) -> StorageFuture<()> {
        let Some(callback) = callback else {
            return Box::pin(async { Ok(()) });
        };
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.transactions.lock().insert(id, tx);
        let operation = self.call(
            "conversation.init",
            object([
                ("callback", encode(&callback)),
                ("tx", encode(&id)),
                ("conversation", encode(&conversation)),
            ]),
        );
        let hub = self.clone();
        Box::pin(async move {
            let result = operation.await;
            hub.transactions.lock().remove(&id);
            result.map(|_| ())
        })
    }
}
impl PromptSection<GenerationPromptInput<ToolHandle, SectionHandle>> for SectionHandle {
    fn tagged(&self) -> bool {
        !self.untagged
    }
    fn render(
        &self,
        input: Arc<GenerationPromptInput<ToolHandle, SectionHandle>>,
        context: &Context,
    ) -> StorageFuture<Option<JsonString>> {
        let Some(hub) = self.hub.upgrade() else {
            return Box::pin(async { Err(Error::message("Section host closed")) });
        };
        let runtime = input.read.erased();
        let id = hub.bind_tool_runtime(&runtime, context);
        let operation = hub.call_context(
            "generation.render",
            object([
                ("section", encode(&self.id)),
                ("runtime", encode(&id)),
                ("taskId", encode(&runtime.task_id())),
                ("conversationId", encode(&input.conversation_id)),
                (
                    "harness",
                    encode(&context.value(&HARNESS_CONTEXT).map(|id| *id)),
                ),
                ("agent", hub.encode_agent(input.agent.clone())),
                ("shown", encode(&input.shown.iter().collect::<Vec<_>>())),
                (
                    "environment",
                    encode(
                        &input
                            .env
                            .as_ref()
                            .and_then(|env| hub.environment.handle(env)),
                    ),
                ),
            ]),
            context,
        );
        Box::pin(async move { operation.await?.as_ref().map(decode).transpose() })
    }
}
