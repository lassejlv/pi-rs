//! The host can replace its model collection without reopening the scheduler.
use crate::*;
use pi_ai::models::Models;
use pi_ai::types::{CacheRetention, DeferredOption, ProviderHeaders, StreamOptions, Transport};
use serde::Deserialize;
use std::sync::Arc;
mod collection;
mod message;
pub use collection::*;
pub use message::*;

pub type ModelsReader = Arc<dyn Fn() -> Result<Models> + Send + Sync>;
pub fn read_models(reader: &ModelsReader) -> Result<Models> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader()))
        .unwrap_or_else(|panic| Err(crate::session::panic_error(panic)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredStreamOptions {
    transport: Option<Transport>,
    timeout_ms: Option<u64>,
    max_retries: Option<u32>,
    max_retry_delay_ms: Option<u64>,
    headers: Option<ProviderHeaders>,
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
    cache_retention: Option<CacheRetention>,
    deferred: Option<DeferredOption>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    reasoning: Option<pi_ai::ThinkingLevel>,
    session_id: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}
/// Convert only at the existing SDK's UTF8 boundary; malformed strings reject
/// instead of silently changing the durable transcript or request options.
pub fn provider_options(value: &JsonObject) -> Result<StreamOptions> {
    let parsed: StoredStreamOptions = wire::decode(value.value()).map_err(Error::message)?;
    Ok(StreamOptions {
        transport: parsed.transport,
        timeout_ms: parsed.timeout_ms,
        max_retries: parsed.max_retries,
        max_retry_delay_ms: parsed.max_retry_delay_ms,
        headers: parsed.headers,
        metadata: parsed.metadata,
        cache_retention: parsed.cache_retention,
        deferred: parsed.deferred,
        temperature: parsed.temperature,
        max_tokens: parsed.max_tokens,
        reasoning: parsed.reasoning,
        session_id: parsed.session_id,
        extra: parsed.extra,
        ..Default::default()
    })
}
/// APIs whose upstream request builders pass every user, assistant, thinking,
/// tool-result and system text through `sanitizeSurrogates`.
const SANITIZING_APIS: [&str; 9] = [
    pi_ai::types::known_api::OPENAI_COMPLETIONS,
    pi_ai::types::known_api::MISTRAL_CONVERSATIONS,
    pi_ai::types::known_api::OPENAI_RESPONSES,
    pi_ai::types::known_api::AZURE_OPENAI_RESPONSES,
    pi_ai::types::known_api::OPENAI_CODEX_RESPONSES,
    pi_ai::types::known_api::ANTHROPIC_MESSAGES,
    pi_ai::types::known_api::BEDROCK_CONVERSE_STREAM,
    pi_ai::types::known_api::GOOGLE_GENERATIVE_AI,
    pi_ai::types::known_api::GOOGLE_VERTEX,
];
/// Strip unpaired surrogates from a string, reporting whether any were removed.
fn sanitize(value: Option<&mut JsonValue>) -> bool {
    let Some(JsonValue::String(text)) = value else {
        return false;
    };
    let sanitized: JsonString =
        pi_ai::utils::sanitize_unicode::sanitize_surrogates_utf16(text.units()).into();
    let removed = sanitized.units().len() != text.units().len();
    *text = sanitized;
    removed
}
/// Which text of a message's `content` lost unpaired surrogates: the whole
/// string content, or each block by position.
#[derive(Default)]
struct Removed {
    text: bool,
    blocks: Vec<bool>,
    sections: Vec<String>,
}
fn sanitize_text_blocks(content: Option<&mut JsonValue>) -> Removed {
    match content {
        Some(text @ JsonValue::String(_)) => Removed {
            text: sanitize(Some(text)),
            ..Default::default()
        },
        Some(content) => Removed {
            text: false,
            sections: Vec::new(),
            blocks: content
                .as_array_mut()
                .into_iter()
                .flatten()
                .map(|block| {
                    let Some(block) = block.as_object_mut() else {
                        return false;
                    };
                    let field = match block.get(&JsonString::from("type")) {
                        Some(kind) if kind.string_equals("text") => "text",
                        Some(kind) if kind.string_equals("thinking") => "thinking",
                        _ => return false,
                    };
                    sanitize(block.get_mut(&JsonString::from(field)))
                })
                .collect(),
        },
        None => Removed::default(),
    }
}
/// Upstream checks a text's length or `trim()` before `sanitizeSurrogates`, so a
/// text that loses unpaired surrogates still counts as non-blank there.
fn mark_removed(message: &mut pi_ai::Message, removed: &Removed) {
    use pi_ai::{AssistantContent, Message, UserContent, UserContentBlock};
    let removed_at = |index: usize| removed.blocks.get(index).copied().unwrap_or(false);
    let mark_blocks = |blocks: &mut [UserContentBlock]| {
        for (index, block) in blocks.iter_mut().enumerate() {
            if let UserContentBlock::Text(text) = block {
                text.lone_surrogates_removed = removed_at(index);
            }
        }
    };
    match message {
        Message::User(user) => match &mut user.content {
            UserContent::Text(_) => user.lone_surrogates_removed = removed.text,
            UserContent::Blocks(blocks) => mark_blocks(blocks),
        },
        Message::Assistant(assistant) => {
            for (index, block) in assistant.content.iter_mut().enumerate() {
                match block {
                    AssistantContent::Text(text) => {
                        text.lone_surrogates_removed = removed_at(index);
                    }
                    AssistantContent::Thinking(thinking) => {
                        thinking.lone_surrogates_removed = removed_at(index);
                    }
                    AssistantContent::ToolCall(_) => {}
                }
            }
        }
        Message::ToolResult(result) => mark_blocks(&mut result.content),
        Message::System(system) => {
            match &mut system.content {
                pi_ai::SystemContent::Text(_) => system.content_lone_surrogates_removed = removed.text,
                pi_ai::SystemContent::Blocks(blocks) => {
                    for (index, pi_ai::SystemTextBlock::Text(text)) in blocks.iter_mut().enumerate() {
                        text.lone_surrogates_removed = removed_at(index);
                    }
                }
            }
            system.sections_lone_surrogates_removed = removed.sections.clone();
        }
    }
}
/// Durable messages keep JavaScript strings; the SDK's request types are UTF8.
/// For APIs that strip unpaired surrogates from request text, strip them here and
/// mark each changed text, so the SDK's blank-text checks see what upstream's
/// checks see before `sanitizeSurrogates`. Other strings, such as tool-call
/// arguments, IDs and names, are sent by upstream as JSON escapes the SDK types
/// cannot carry, so they still reject instead of changing.
pub fn provider_messages(messages: &[Arc<JsonValue>], api: &str) -> Result<Vec<pi_ai::Message>> {
    let sanitizing = SANITIZING_APIS.contains(&api);
    messages
        .iter()
        .map(|message| {
            // Codex sends the leading system text as `instructions` without
            // `sanitizeSurrogates`, so its system messages keep rejecting.
            let unsanitized_system = api == pi_ai::types::known_api::OPENAI_CODEX_RESPONSES
                && message["role"].string_equals("system");
            if !sanitizing || unsanitized_system {
                return wire::decode(message).map_err(Error::message);
            }
            let mut message = JsonValue::clone(message);
            let mut removed = Removed::default();
            if let Some(object) = message.as_object_mut() {
                removed = sanitize_text_blocks(object.get_mut(&JsonString::from("content")));
                if let Some(sections) = object
                    .get_mut(&JsonString::from("sections"))
                    .and_then(JsonValue::as_object_mut)
                {
                    for (name, section) in sections.iter_mut() {
                        if sanitize(Some(section)) {
                            removed.sections.push(name.to_string_lossy());
                        }
                    }
                }
            }
            let mut decoded = wire::decode(&message).map_err(Error::message)?;
            mark_removed(&mut decoded, &removed);
            Ok(decoded)
        })
        .collect()
}
/// Reject values the current SDK cannot represent, rather than saturating a
/// negative, fractional or overflowing JavaScript number during a Rust cast.
pub fn provider_token_limit(value: f64) -> Result<u64> {
    wire::decode(&wire::encode(&value)).map_err(Error::message)
}
/// A request owns its cancellation bridge. Settlement or a dropped caller
/// releases the watcher and cancels only this provider request.
pub struct RequestScope {
    token: tokio_util::sync::CancellationToken,
    watch: pi_chord::rt::JoinHandle<()>,
}
impl RequestScope {
    pub fn new(
        options: &mut StreamOptions,
        signal: pi_chord::AbortSignal,
        context: &Context,
    ) -> Self {
        let token = tokio_util::sync::CancellationToken::new();
        if signal.aborted() {
            token.cancel();
        }
        let pending = token.clone();
        let watch = pi_chord::rt::spawn(async move {
            signal.cancelled().await;
            pending.cancel();
        });
        options.signal = Some(token.clone());
        options.telemetry = Some(context.telemetry().clone());
        Self { token, watch }
    }
}
impl Drop for RequestScope {
    fn drop(&mut self) {
        self.token.cancel();
        self.watch.abort();
    }
}
