use super::*;
use pi_durable::harness::{compaction, context::ContextView};
use std::sync::Weak;
pub(super) struct HostCompactionHooks {
    pub hub: Weak<Hub>,
    pub id: u64,
}
impl compaction::CompactionHookProvider for HostCompactionHooks {
    fn before_compact(&self) -> Result<Option<compaction::BeforeCompact>> {
        let hub = self
            .hub
            .upgrade()
            .ok_or_else(|| Error::message("Host closed"))?;
        let callback: Option<u64> = decode(&hub.sync(
            "hookMember",
            object([
                ("handlers", encode(&self.id)),
                ("name", encode(&"beforeCompact")),
            ]),
        )?)?;
        Ok(callback.map(|callback| {
            Arc::new(
                move |source: Arc<compaction::CompactionSource>,
                      runtime: compaction::CompactionRuntime,
                      context: Context|
                      -> StorageFuture<Option<compaction::CompactionDecision>> {
                    hub.models.compaction_hooks.fetch_add(1, Ordering::Relaxed);
                    let operation = hub.call_context(
                        "compactionHook",
                        object([
                            ("callback", encode(&callback)),
                            ("source", encode(&source)),
                            (
                                "runtime",
                                encode(&hub.bind_tool_runtime(&runtime.erased(), &context)),
                            ),
                            ("taskId", encode(&runtime.task_id())),
                            ("conversationId", encode(&runtime.conversation_id())),
                            (
                                "harness",
                                encode(&context.value(&HARNESS_CONTEXT).map(|id| *id)),
                            ),
                        ]),
                        &context,
                    );
                    Box::pin(async move {
                        operation
                            .await?
                            .map(|value| {
                                if value.as_object().is_some_and(|value| {
                                    value.contains_key(&JsonString::from("decline"))
                                }) {
                                    Ok(compaction::CompactionDecision::Decline)
                                } else {
                                    field(&value, "summary")
                                        .map(compaction::CompactionDecision::Summary)
                                }
                            })
                            .transpose()
                    })
                },
            ) as compaction::BeforeCompact
        }))
    }
}
impl Hub {
    pub(super) fn compaction_request(&self, request: &JsonValue) -> Result<Option<JsonValue>> {
        self.compaction_reads.fetch_add(1, Ordering::Relaxed);
        Ok(match request["method"].to_utf8().as_deref() {
            Some("compaction.serialize") => {
                Some(encode(&compaction::serialize_conversation(&field::<
                    Vec<Arc<JsonValue>>,
                >(
                    request, "messages",
                )?)))
            }
            Some("compaction.select") => {
                let value = &request["view"];
                let view = ContextView {
                    head: field(value, "head")?,
                    entries: field(value, "entries")?,
                    contributions: field(value, "contributions")?,
                    messages: field(value, "messages")?,
                };
                compaction::select_cut(&view, field(request, "budget")?).map(|index| encode(&index))
            }
            other => {
                return Err(Error::message(format!(
                    "Unknown compaction method {other:?}"
                )));
            }
        })
    }
}
