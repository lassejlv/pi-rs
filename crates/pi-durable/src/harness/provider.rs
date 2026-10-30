//! Stable provider identity, fresh for each conversation and fork.
use crate::*;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderState {
    pub session_id: JsonString,
}
impl DocumentState for ProviderState {}
pub static PROVIDER_DOC: std::sync::LazyLock<LatestConversationDocToken<ProviderState>> =
    std::sync::LazyLock::new(|| {
        define_doc(DocDefinition::new(
            "pi.provider",
            1,
            LatestConversationScope {
                fork: LatestFork::Initial,
            },
            || {
                Ok(ProviderState {
                    session_id: pi_ai::utils::uuid::try_uuidv7(None)
                        .map_err(|error| Error::message(error.to_string()))?
                        .into(),
                })
            },
            |_, _, _| true,
        ))
        .expect("valid built-in provider definition")
    });
pub fn provider_doc() -> LatestConversationDocToken<ProviderState> {
    PROVIDER_DOC.clone()
}
pub async fn ensure_provider_session_id<
    I: TaskValue,
    S: TaskValue,
    R: TaskValue,
    H: Send + Sync + 'static,
>(
    runtime: &TaskRuntime<I, S, R, H>,
    context: &Context,
) -> Result<JsonString> {
    let conversation = runtime.conversation_id();
    if let Some(existing) = runtime
        .snapshot(&provider_doc(), conversation, context)
        .await?
    {
        return Ok(existing.session_id.clone());
    }
    let captured = Arc::new(Mutex::new(None));
    let capture = captured.clone();
    runtime
        .commit(
            move |tx, _| async move {
                *capture.lock() = Some(
                    tx.doc(&provider_doc(), conversation)
                        .await?
                        .read()?
                        .session_id,
                );
                Ok(None)
            },
            context,
        )
        .await?;
    let created = captured.lock().take();
    created.ok_or_else(|| {
        Error::message(format!(
            "Conversation {conversation} has no provider session ID"
        ))
    })
}
