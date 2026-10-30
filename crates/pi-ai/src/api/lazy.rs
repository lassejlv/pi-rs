//! Deferred stream setup (port of `api/lazy.ts`).

use std::sync::Arc;

use futures::future::BoxFuture;

use crate::types::{AssistantMessage, AssistantMessageEvent, Model, StopReason, Usage};
use crate::utils::event_stream::AssistantMessageEventStream;

/// Error message for failures before a provider stream exists.
pub fn create_setup_error_message(model: &Model, error: &anyhow::Error, timestamp: i64) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some(error.to_string()),
        timestamp,
        ..Default::default()
    }
}

/// Synchronous, read-only observation of a lazy stream's forwarding path.
#[derive(Clone, Default)]
pub struct LazyStreamObserver {
    pub on_event: Option<OnLazyEvent>,
    pub on_result: Option<OnLazyResult>,
}

pub type OnLazyEvent = Arc<dyn Fn(&AssistantMessageEvent) + Send + Sync>;
pub type OnLazyResult = Arc<dyn Fn(Option<&AssistantMessage>) + Send + Sync>;

/// Returns a stream immediately while running async setup (auth resolution, ...) behind it.
/// Setup failures terminate the stream with an error event. Must be called within a tokio runtime.
pub fn lazy_stream(
    model: &Model,
    setup: BoxFuture<'static, anyhow::Result<AssistantMessageEventStream>>,
    observer: Option<LazyStreamObserver>,
) -> AssistantMessageEventStream {
    let started_at = crate::utils::now_ms();
    let (sender, outer) = AssistantMessageEventStream::new();
    let model = model.clone();
    crate::utils::wasm::spawn(async move {
        let observer = observer.unwrap_or_default();
        match setup.await {
            Ok(mut inner) => {
                let mut result: Option<AssistantMessage> = None;
                while let Some(event) = inner.next_event().await {
                    match &event {
                        AssistantMessageEvent::Done { message, .. } => result = Some(message.clone()),
                        AssistantMessageEvent::Error { error, .. } => result = Some(error.clone()),
                        _ => {}
                    }
                    match &observer.on_event {
                        Some(on_event) => {
                            sender.push(event.clone());
                            on_event(&event);
                        }
                        None => sender.push(event),
                    }
                }
                if let Some(final_result) = inner.try_result().await {
                    result = Some(final_result);
                }
                sender.end(result.clone());
                if let Some(on_result) = &observer.on_result {
                    on_result(result.as_ref());
                }
            }
            Err(error) => {
                let message = create_setup_error_message(&model, &error, started_at);
                let event = AssistantMessageEvent::Error { reason: StopReason::Error, error: message.clone() };
                sender.push(event.clone());
                if let Some(on_event) = &observer.on_event {
                    on_event(&event);
                }
                sender.end(Some(message.clone()));
                if let Some(on_result) = &observer.on_result {
                    on_result(Some(&message));
                }
            }
        }
    });
    outer
}

/// A stream that immediately fails with `error`.
pub fn error_stream(model: &Model, error: anyhow::Error) -> AssistantMessageEventStream {
    lazy_stream(model, Box::pin(async move { Err(error) }), None)
}
