//! Port of `packages/coding-agent/src/modes/interactive/components/markdown-transform.ts`.

use crate::core::extensions::types::{MarkdownMessageType, MarkdownTransformContext, MarkdownTransformer};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CreateMarkdownTransformMessageType {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "assistant-thinking")]
    AssistantThinking,
}

impl CreateMarkdownTransformMessageType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::AssistantThinking => "assistant-thinking",
        }
    }

    fn context_type(self) -> MarkdownMessageType {
        match self {
            Self::User => MarkdownMessageType::User,
            Self::Assistant => MarkdownMessageType::Assistant,
            Self::AssistantThinking => MarkdownMessageType::AssistantThinking,
        }
    }
}

pub fn create_markdown_transform(
    message_type: CreateMarkdownTransformMessageType,
    is_streaming: bool,
    transformers: &[MarkdownTransformer],
    message_id: Option<&str>,
    timestamp: Option<&str>,
) -> Arc<dyn Fn(String, i64) -> String + Send + Sync> {
    let transformers: Vec<MarkdownTransformer> = transformers.to_vec();
    let message_id = message_id.map(str::to_owned);
    let timestamp = timestamp.map(str::to_owned);
    Arc::new(move |markdown: String, available_width: i64| {
        let context = MarkdownTransformContext {
            message_type: message_type.context_type(),
            is_streaming,
            available_width: available_width.max(0) as usize,
            message_id: message_id.clone(),
            timestamp: timestamp.clone(),
        };
        apply_markdown_transformers(markdown, &context, &transformers)
    })
}

/// [`create_markdown_transform`] as the transform hook of pi-tui's `Markdown`.
pub(crate) fn markdown_transform_fn(
    message_type: CreateMarkdownTransformMessageType,
    is_streaming: bool,
    transformers: &[MarkdownTransformer],
    message_id: Option<&str>,
    timestamp: Option<&str>,
) -> pi_tui::components::markdown::MarkdownTransformFn {
    let transform = create_markdown_transform(message_type, is_streaming, transformers, message_id, timestamp);
    Arc::new(move |markdown: &str, width: usize| transform(markdown.to_string(), width as i64))
}

fn apply_markdown_transformers(
    markdown: String,
    context: &MarkdownTransformContext,
    transformers: &[MarkdownTransformer],
) -> String {
    let mut transformed_markdown = markdown;
    for transformer in transformers {
        // A transformer that fails (panics) keeps the current Markdown; the chain continues.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            transformer(&transformed_markdown, context)
        }));
        if let Ok(transformed) = result {
            transformed_markdown = transformed;
        }
    }
    transformed_markdown
}
