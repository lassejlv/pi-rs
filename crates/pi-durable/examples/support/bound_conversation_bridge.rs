//! Invocation-bound conversation capabilities preserve native admission and cancellation.
use super::*;
use pi_durable::harness::conversation::{
    BoundSubmission, ConversationHandle, InputSubmissionDraft,
};
#[derive(Default)]
pub(super) struct BoundBridge {
    conversations: Mutex<HashMap<u64, ConversationHandle>>,
    submissions: Mutex<HashMap<u64, BoundSubmission>>,
}
impl Hub {
    pub(super) fn bound_request(
        self: &Arc<Self>,
        method: &str,
        request: JsonValue,
        context: Context,
    ) -> Result<StorageFuture<Option<JsonValue>>> {
        if method == "runtimeConversation" {
            let operation = self
                .runtime(&request)?
                .conversation(field(&request, "conversation")?, &context);
            let hub = self.clone();
            return Ok(Box::pin(async move {
                Ok(operation.await?.map(|conversation| {
                    let handle = hub.next.fetch_add(1, Ordering::Relaxed);
                    let id = conversation.id;
                    hub.bound.conversations.lock().insert(handle, conversation);
                    object([("handle", encode(&handle)), ("id", encode(&id))])
                }))
            }));
        }
        let handle = field::<u64>(&request, "handle")?;
        if method.starts_with("bound.submission.") {
            let submission = self
                .bound
                .submissions
                .lock()
                .get(&handle)
                .cloned()
                .ok_or_else(|| Error::message("Unknown bound submission"))?;
            return Ok(match method {
                "bound.submission.status" => {
                    let operation = submission.status(&context);
                    Box::pin(async move { Ok(Some(encode(&operation.await?))) })
                }
                "bound.submission.wait" => {
                    let operation = submission.wait(&context);
                    Box::pin(async move { Ok(Some(encode(&operation.await?))) })
                }
                "bound.submission.abort" => {
                    let operation = submission.abort(&context);
                    Box::pin(async move { Ok(Some(encode(&operation.await?))) })
                }
                _ => return Err(Error::message("Unknown bound submission operation")),
            });
        }
        let conversation = self
            .bound
            .conversations
            .lock()
            .get(&handle)
            .cloned()
            .ok_or_else(|| Error::message("Unknown bound conversation"))?;
        Ok(match method {
            "bound.submit" => {
                let draft: SubmissionDraft = submission_bridge::decode_draft(&request["draft"])?;
                let SubmissionDraft::Input {
                    request_id,
                    content,
                    when_busy,
                    entry,
                } = draft
                else {
                    return Err(Error::message(
                        "Invocation conversations accept input submissions",
                    ));
                };
                let operation = conversation.submit(
                    InputSubmissionDraft {
                        request_id,
                        content,
                        when_busy,
                        entry,
                    },
                    &context,
                );
                let hub = self.clone();
                Box::pin(async move {
                    let submission = operation.await?;
                    let id = submission.id;
                    let handle = hub.next.fetch_add(1, Ordering::Relaxed);
                    hub.bound.submissions.lock().insert(handle, submission);
                    Ok(Some(object([
                        ("handle", encode(&handle)),
                        ("id", encode(&id)),
                    ])))
                })
            }
            "bound.abort" => {
                let operation =
                    conversation.abort(&context, request["background"].as_bool() == Some(true));
                Box::pin(async move {
                    operation.await?;
                    Ok(None)
                })
            }
            "bound.waitForIdle" => {
                let operation = conversation.wait_for_idle(&context);
                Box::pin(async move {
                    operation.await?;
                    Ok(None)
                })
            }
            _ => return Err(Error::message("Unknown bound conversation operation")),
        })
    }
}
