use crate::harness::{
    agent::Agent,
    compaction::CompactionResult,
    hooks::HookKey,
    prompt::PromptInput,
    tool::ToolTaskResult,
    types::{ModelRef, UserInput},
};
use crate::{wire, *};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock};

pub type GenerationInput = JsonObject;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationResult {
    pub entry_id: EntryId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preparation {
    pub attempt: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted: Option<TypedTaskId<CompactionResult>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow: Option<JsonString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<bool>,
}
impl Preparation {
    pub fn initial() -> Self {
        Self {
            attempt: 1,
            compacted: None,
            overflow: None,
            threshold: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationRequest {
    pub attempt: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted: Option<TypedTaskId<CompactionResult>>,
    pub model: ModelRef,
    pub thinking_level: JsonString,
    pub stream_options: JsonObject,
    pub cutoff: EntryId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Backoff {
    pub attempt: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted: Option<TypedTaskId<CompactionResult>>,
    pub until: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredPoll {
    pub attempt: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted: Option<TypedTaskId<CompactionResult>>,
    pub model: ModelRef,
    pub cutoff: EntryId,
    pub handle: JsonObject,
    pub poll_at: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolRound {
    pub assistant: EntryId,
    pub tools: Vec<TypedTaskId<ToolTaskResult>>,
    pub pending: Vec<JsonString>,
}
#[derive(Clone, Debug)]
pub enum GenerationCheckpoint {
    Prepare(Preparation),
    Request(GenerationRequest),
    Retry(Backoff),
    Poll(DeferredPoll),
    Tools(ToolRound),
}
impl GenerationCheckpoint {
    fn to_json(&self) -> JsonValue {
        let (phase, fields) = match self {
            Self::Prepare(v) => ("prepare", wire::encode(v)),
            Self::Request(v) => ("request", wire::encode(v)),
            Self::Retry(v) => ("retry", wire::encode(v)),
            Self::Poll(v) => ("poll", wire::encode(v)),
            Self::Tools(v) => ("tools", wire::encode(v)),
        };
        let mut value = wire::object();
        wire::put(&mut value, "phase", &phase);
        value
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        Ok(match wire::tag(value, "phase")?.as_str() {
            "prepare" => Self::Prepare(wire::decode(value)?),
            "request" => Self::Request(wire::decode(value)?),
            "retry" => Self::Retry(wire::decode(value)?),
            "poll" => Self::Poll(wire::decode(value)?),
            "tools" => Self::Tools(wire::decode(value)?),
            phase => return Err(format!("Unknown generation phase {phase}")),
        })
    }
}
wire::raw_serde!(GenerationCheckpoint);
pub type GenerationRuntime =
    TaskRuntime<GenerationInput, GenerationCheckpoint, GenerationResult, GenerationHooks>;
pub type GenerationTask =
    Task<GenerationInput, GenerationCheckpoint, GenerationResult, GenerationHooks>;
pub type GenerationPromptInput<T, S> =
    PromptInput<Agent<T, S>, GenerationRuntime, dyn crate::env::ExecutionEnv>;
pub type GenerationMessage = crate::harness::models::AssistantValueRef;
pub type BeforeRequest = Arc<
    dyn Fn(
            Vec<Arc<JsonValue>>,
            GenerationRuntime,
            Context,
        ) -> storage::StorageFuture<Option<Vec<Arc<JsonValue>>>>
        + Send
        + Sync,
>;
pub type AfterResponse = Arc<
    dyn Fn(GenerationMessage, GenerationRuntime, Context) -> storage::StorageFuture<()>
        + Send
        + Sync,
>;
pub type OnYield = Arc<
    dyn Fn(
            GenerationMessage,
            GenerationRuntime,
            Context,
        ) -> storage::StorageFuture<Option<UserInput>>
        + Send
        + Sync,
>;
pub type AfterTools = Arc<
    dyn Fn(EntryId, Vec<EntryId>, GenerationRuntime, Context) -> storage::StorageFuture<()>
        + Send
        + Sync,
>;
#[derive(Default)]
pub struct GenerationHooks {
    pub before_request: Option<BeforeRequest>,
    pub after_response: Option<AfterResponse>,
    pub on_yield: Option<OnYield>,
    pub after_tools: Option<AfterTools>,
    pub provider: Option<Arc<dyn GenerationHookProvider>>,
}
pub trait GenerationHookProvider: Send + Sync {
    fn before_request(&self) -> Result<Option<BeforeRequest>> {
        Ok(None)
    }
    fn after_response(&self) -> Result<Option<AfterResponse>> {
        Ok(None)
    }
    fn on_yield(&self) -> Result<Option<OnYield>> {
        Ok(None)
    }
    fn after_tools(&self) -> Result<Option<AfterTools>> {
        Ok(None)
    }
}
pub(super) static BEFORE_REQUEST: LazyLock<HookKey<GenerationHooks, BeforeRequest>> =
    LazyLock::new(|| {
        HookKey::try_new("beforeRequest", |h: Arc<GenerationHooks>| {
            h.provider
                .as_ref()
                .map_or_else(|| Ok(h.before_request.clone()), |p| p.before_request())
        })
    });
pub(super) static AFTER_RESPONSE: LazyLock<HookKey<GenerationHooks, AfterResponse>> =
    LazyLock::new(|| {
        HookKey::try_new("afterResponse", |h: Arc<GenerationHooks>| {
            h.provider
                .as_ref()
                .map_or_else(|| Ok(h.after_response.clone()), |p| p.after_response())
        })
    });
pub(super) static ON_YIELD: LazyLock<HookKey<GenerationHooks, OnYield>> = LazyLock::new(|| {
    HookKey::try_new("onYield", |h: Arc<GenerationHooks>| {
        h.provider
            .as_ref()
            .map_or_else(|| Ok(h.on_yield.clone()), |p| p.on_yield())
    })
});
pub(super) static AFTER_TOOLS: LazyLock<HookKey<GenerationHooks, AfterTools>> =
    LazyLock::new(|| {
        HookKey::try_new("afterTools", |h: Arc<GenerationHooks>| {
            h.provider
                .as_ref()
                .map_or_else(|| Ok(h.after_tools.clone()), |p| p.after_tools())
        })
    });
pub(super) type Running = RunningTask<GenerationInput, GenerationCheckpoint, GenerationResult>;
pub(super) type Next = NextTaskState<GenerationCheckpoint, GenerationResult>;
#[derive(Clone)]
pub(super) struct ResponseRequest {
    pub attempt: u64,
    pub compacted: Option<TypedTaskId<CompactionResult>>,
    pub model: ModelRef,
    pub cutoff: EntryId,
    pub messages: Option<Vec<Arc<JsonValue>>>,
    pub poll_at: Option<f64>,
}
pub(super) fn resolve(
    runtime: &GenerationRuntime,
    reference: &ModelRef,
) -> Result<Option<crate::harness::models::ResolvedModel>> {
    runtime.model_collection()?.get_model(reference)
}
pub(super) fn joined(parts: impl IntoIterator<Item = JsonString>) -> JsonString {
    let mut text = JsonString::default();
    for part in parts {
        text.append(&part);
    }
    text
}
