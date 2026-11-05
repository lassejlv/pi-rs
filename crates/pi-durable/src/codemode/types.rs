use super::{describe::CodemodeNamespace, vm::Sandbox};
use crate::{harness::tool::ToolExecutionApi, storage::StorageFuture, *};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock};

pub const CODEMODE_MODULE_VERSION: u64 = 2;
pub const CODEMODE_TOOL_NAME: &str = "codemode";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredTool {
    pub name: JsonString,
    pub js_name: JsonString,
    pub description: JsonString,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub structured: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeclaredGlobal {
    pub name: JsonString,
    pub spread: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartNamespace {
    pub name: JsonString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<JsonString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<JsonString>,
    pub tools: Vec<JsonString>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Start {
    pub version: u64,
    pub started_at: f64,
    pub seed: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<JsonString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<f64>,
    pub tools: Vec<DeclaredTool>,
    pub namespaces: Vec<StartNamespace>,
    pub globals: Vec<DeclaredGlobal>,
    pub max_output_tokens: f64,
    pub store: JsonObject,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<Start>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<JsonString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<JsonObject>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GlobalEntry {
    pub name: JsonString,
    #[serde(
        default,
        deserialize_with = "present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub args: Option<JsonValue>,
    pub outcome: JsonObject,
}
pub(crate) fn present_json<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<JsonValue>, D::Error> {
    JsonValue::deserialize(deserializer).map(Some)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    pub id: u64,
    pub at: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global: Option<GlobalEntry>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalState {
    pub entries: Vec<JournalEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_issued: Option<u64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoreState {
    pub values: JsonObject,
}
impl DocumentState for RunState {}
impl DocumentState for JournalState {}
impl DocumentState for StoreState {}
pub static CODEMODE_RUN_DOC: LazyLock<TaskDocToken<RunState>> = LazyLock::new(|| {
    define_doc(DocDefinition::new(
        "pi.codemode.run",
        1,
        TaskScope,
        || Ok(RunState::default()),
        |_, _, _| false,
    ))
    .expect("valid built-in code-mode run")
});
pub static CODEMODE_JOURNAL_DOC: LazyLock<TaskDocToken<JournalState>> = LazyLock::new(|| {
    define_doc(DocDefinition::new(
        "pi.codemode.journal",
        1,
        TaskScope,
        || Ok(JournalState::default()),
        |_, _, _| false,
    ))
    .expect("valid built-in code-mode journal")
});
pub static CODEMODE_STORE_DOC: LazyLock<RewindableConversationDocToken<StoreState>> =
    LazyLock::new(|| {
        define_doc(DocDefinition::new(
            "pi.codemode.store",
            1,
            RewindableConversationScope {
                fork: ForkPolicy::AsOf,
            },
            || Ok(StoreState::default()),
            |_, _, info| info.deltas_since_base >= 64,
        ))
        .expect("valid built-in code-mode store")
    });
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeNestedCall {
    pub id: u64,
    pub name: JsonString,
    pub args: JsonString,
    pub status: JsonString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonString>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeToolDetails {
    pub calls: Vec<CodemodeNestedCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<JsonString>,
}
#[derive(Clone)]
pub struct CodemodeGlobal {
    pub name: JsonString,
    pub spread: bool,
    pub execute: Arc<
        dyn Fn(Option<JsonValue>, ToolExecutionApi, Context) -> StorageFuture<Option<JsonValue>>
            + Send
            + Sync,
    >,
}
#[derive(Clone)]
pub struct CodemodeToolOptions {
    pub sandbox: Arc<dyn Sandbox>,
    pub namespaces: Vec<CodemodeNamespace>,
    pub inline_budget: f64,
    pub models: bool,
    pub globals: Vec<CodemodeGlobal>,
}
impl CodemodeToolOptions {
    pub fn new(sandbox: Arc<dyn Sandbox>) -> Self {
        Self {
            sandbox,
            namespaces: vec![],
            inline_budget: super::describe::DEFAULT_CODEMODE_INLINE_BUDGET,
            models: true,
            globals: vec![],
        }
    }
}
