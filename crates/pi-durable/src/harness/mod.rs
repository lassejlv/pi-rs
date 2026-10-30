//! Durable task orchestration services and their derived live-state indexes.
pub mod agent;
pub mod catalog;
pub mod compaction;
pub mod context;
pub mod conversation;
pub mod declaration;
pub mod define;
pub mod events;
#[path = "harness.rs"]
mod facade;
pub mod generation;
pub mod hook_api;
pub mod hooks;
pub use facade::{
    Conversation, ConversationCreateOptions, ConversationCreated, ConversationEntryQuery,
    ConversationInit, ConversationOptions, EnvTarget, EnvironmentFactory, Harness, HarnessOptions,
    HarnessReport, HarnessSettingsReader, HostClock, create_registry, default_harness_identity,
    safe_now,
};
pub mod inbox;
pub mod inspect;
pub mod json;
pub mod live;
pub mod models;
pub mod ownership;
pub mod prompt;
pub mod provider;
pub mod reader;
pub mod registry;
pub mod runtime;
pub mod scheduler;
pub mod settled;
pub mod settings;
pub mod state;
pub mod submissions;
pub mod task_graph;
pub mod telemetry;
pub mod tool;
pub mod typed;
pub mod types;
pub mod usage;
pub mod view;

/// Built-in documents are created before the host's conversation-created hook.
pub async fn create_conversation_documents(
    tx: &crate::Transaction,
    record: crate::ConversationRecord,
) -> crate::Result<()> {
    tx.doc(&live::live_doc(), record.id).await?;
    tx.doc(&inbox::inbox_doc(), record.id).await?;
    tx.doc(&usage::usage_doc(), record.id).await?;
    tx.doc(&provider::provider_doc(), record.id).await?;
    agent::create_agent(tx, record).await
}
pub mod util;
