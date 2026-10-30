//! Typed forms of the Harness configuration values that the entry points take as JSON
//! (`AgentChange`, `ConversationStreamOptions`, `ConversationRetryPolicy`,
//! `CompactionSettings`, `AfterFailedRun`), matching upstream `harness/types.ts`.
//!
//! The JSON entry points are unchanged. Every typed value converts to and from the JSON
//! objects they accept, and upstream's runtime behaviour toward unknown fields is kept:
//! - an agent change reads only its known keys, so unknown keys have no effect;
//! - stream, retry and compaction settings are spread into the resolved settings, so unknown
//!   keys are kept (`extra`) and round-trip.

use super::{
    compaction::CompactionPolicy,
    settings::HarnessSettings,
    types::ModelRef,
    {Conversation, ConversationCreateOptions, ConversationOptions},
};
use crate::{wire, *};
use indexmap::IndexMap;
use pi_ai::{
    providers::model_schema::ModelThinkingLevel,
    types::{CacheRetention, DeferredOption, Transport},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

type Extra = serde_json::Map<String, serde_json::Value>;

/// A field of a change: absent leaves it alone, `null` clears it, a value replaces it.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Change<T> {
    #[default]
    Unset,
    Clear,
    Set(T),
}
impl<T> Change<T> {
    pub fn is_unset(&self) -> bool {
        matches!(self, Self::Unset)
    }
}
impl<T: Serialize> Serialize for Change<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Set(value) => value.serialize(serializer),
            Self::Unset | Self::Clear => serializer.serialize_none(),
        }
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Change<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Set(value),
            None => Self::Clear,
        })
    }
}

/// A selection of extensions or model tools by name: the whole list, or an edit of the stored one.
/// On the wire each name is an object with a `name`, as upstream passes registered objects.
#[derive(Clone, Debug, PartialEq)]
pub enum NameSelection {
    List(Vec<JsonString>),
    Edit {
        add: Option<Vec<JsonString>>,
        remove: Option<Vec<JsonString>>,
    },
}
fn name_list(value: &serde_json::Value) -> std::result::Result<Vec<JsonString>, String> {
    let items = value.as_array().ok_or("Expected named choices")?;
    items
        .iter()
        .map(|item| {
            item.get("name")
                .ok_or_else(|| "Choice has no name".to_owned())
                .and_then(|name| {
                    serde_json::from_value(name.clone()).map_err(|error| error.to_string())
                })
        })
        .collect()
}
fn name_objects(names: &[JsonString]) -> serde_json::Value {
    names
        .iter()
        .map(|name| serde_json::json!({ "name": name }))
        .collect()
}
fn part(
    value: &serde_json::Value,
    key: &str,
) -> std::result::Result<Option<Vec<JsonString>>, String> {
    value.get(key).map(name_list).transpose()
}
impl Serialize for NameSelection {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::List(list) => name_objects(list),
            Self::Edit { add, remove } => {
                let mut map = serde_json::Map::new();
                if let Some(add) = add {
                    map.insert("add".into(), name_objects(add));
                }
                if let Some(remove) = remove {
                    map.insert("remove".into(), name_objects(remove));
                }
                map.into()
            }
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for NameSelection {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let parsed = if value.is_array() {
            name_list(&value).map(Self::List)
        } else if value.is_object() {
            part(&value, "add")
                .and_then(|add| part(&value, "remove").map(|remove| Self::Edit { add, remove }))
        } else {
            Err("Expected a list or an edit".into())
        };
        parsed.map_err(serde::de::Error::custom)
    }
}

/// The tools of an agent: the whole list, or the names to remove from the enabled set.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolSelection {
    List(Vec<JsonString>),
    Remove(Vec<JsonString>),
}
impl Serialize for ToolSelection {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::List(list) => name_objects(list),
            Self::Remove(remove) => serde_json::json!({ "remove": name_objects(remove) }),
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for ToolSelection {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let parsed = if value.is_array() {
            name_list(&value).map(Self::List)
        } else if value.is_object() {
            part(&value, "remove").and_then(|remove| {
                remove
                    .map(Self::Remove)
                    .ok_or_else(|| "Missing remove".to_owned())
            })
        } else {
            Err("Expected a list or a removal".into())
        };
        parsed.map_err(serde::de::Error::custom)
    }
}

/// The model of an agent change. Upstream stores a copy of the whole object it is given
/// (`{ ...change.model }`), so fields beyond `provider` and `modelId` persist in `pi.agent`;
/// they are kept in `extra`.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelChange {
    pub model: ModelRef,
    pub extra: Extra,
}
impl Serialize for ModelChange {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serde_json::Map::new();
        map.insert(
            "provider".into(),
            serde_json::to_value(&self.model.provider).map_err(serde::ser::Error::custom)?,
        );
        map.insert(
            "modelId".into(),
            serde_json::to_value(&self.model.model_id).map_err(serde::ser::Error::custom)?,
        );
        for (key, value) in &self.extra {
            map.entry(key.clone()).or_insert_with(|| value.clone());
        }
        map.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for ModelChange {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let mut map = Extra::deserialize(deserializer)?;
        let mut take = |key: &str| {
            map.remove(key)
                .ok_or_else(|| serde::de::Error::custom(format!("Missing field {key}")))
                .and_then(|value| serde_json::from_value(value).map_err(serde::de::Error::custom))
        };
        let provider = take("provider")?;
        let model_id = take("modelId")?;
        Ok(Self {
            model: ModelRef { provider, model_id },
            extra: map,
        })
    }
}
impl From<ModelRef> for ModelChange {
    fn from(model: ModelRef) -> Self {
        Self {
            model,
            extra: Extra::new(),
        }
    }
}

/// A change to `pi.agent`: a given field replaces the stored one, `Clear` clears it, `Unset` changes nothing.
/// Unknown keys of an incoming change are ignored, as upstream reads only the fields below.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChange {
    #[serde(default, skip_serializing_if = "Change::is_unset")]
    pub model: Change<ModelChange>,
    #[serde(default, skip_serializing_if = "Change::is_unset")]
    pub thinking_level: Change<ModelThinkingLevel>,
    #[serde(default, skip_serializing_if = "Change::is_unset")]
    pub extensions: Change<NameSelection>,
    #[serde(default, skip_serializing_if = "Change::is_unset")]
    pub tools: Change<ToolSelection>,
    #[serde(default, skip_serializing_if = "Change::is_unset")]
    pub model_tools: Change<NameSelection>,
    #[serde(default, skip_serializing_if = "Change::is_unset")]
    pub instructions: Change<JsonString>,
    #[serde(default, skip_serializing_if = "Change::is_unset")]
    pub cwd: Change<JsonString>,
}
impl TryFrom<&AgentChange> for JsonObject {
    type Error = Error;
    fn try_from(change: &AgentChange) -> Result<Self> {
        JsonObject::try_from_value(wire::encode(change)).map_err(Error::message)
    }
}
impl TryFrom<&JsonObject> for AgentChange {
    type Error = Error;
    fn try_from(change: &JsonObject) -> Result<Self> {
        wire::decode(change.value()).map_err(Error::message)
    }
}

/// `deferred` of stream options: on or off, or a retention window.
pub type Deferred = DeferredOption;

/// Curated pi-ai request options; absent fields use pi-ai defaults. Other keys are kept in `extra`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationStreamOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<f64>,
    /// Provider/SDK retries inside one request attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Extra>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<CacheRetention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<Deferred>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Durable generation attempt retries; the JSON shape of pi-ai `RetryPolicy`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRetryPolicy {
    pub enabled: bool,
    pub max_retries: f64,
    pub base_delay_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_delay_ms: Option<f64>,
    #[serde(flatten)]
    pub extra: Extra,
}
/// `Partial<ConversationRetryPolicy>`, as `HarnessSettings.retry` takes it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRetryPolicyPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_delay_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_delay_ms: Option<f64>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `Partial<CompactionPolicy>`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionPolicyPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_tokens: Option<f64>,
}
/// Harness-wide compaction settings. `model_overrides` is keyed by exact `"provider/modelId"`; each field
/// it sets replaces the harness-wide one for that model.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    #[serde(flatten)]
    pub policy: CompactionPolicyPatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<IndexMap<String, CompactionPolicyPatch>>,
    #[serde(flatten)]
    pub extra: Extra,
}
impl From<CompactionPolicy> for CompactionPolicyPatch {
    fn from(policy: CompactionPolicy) -> Self {
        Self {
            enabled: Some(policy.enabled),
            reserve_tokens: Some(policy.reserve_tokens),
            keep_recent_tokens: Some(policy.keep_recent_tokens),
            background_tokens: Some(policy.background_tokens),
        }
    }
}

/// What a failed run does with the inputs queued behind it (`HarnessSettings.afterFailedRun`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AfterFailedRun {
    /// Leave them queued until the next input or queued write. The default.
    #[default]
    Keep,
    /// Start the next run with them, as an answer would.
    Continue,
    /// Settle them `unanswered` with reason `run_failed`.
    Withdraw,
}
impl AfterFailedRun {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Continue => "continue",
            Self::Withdraw => "withdraw",
        }
    }
}
impl TryFrom<&JsonString> for AfterFailedRun {
    type Error = Error;
    fn try_from(value: &JsonString) -> Result<Self> {
        [Self::Keep, Self::Continue, Self::Withdraw]
            .into_iter()
            .find(|candidate| value.equals_str(candidate.as_str()))
            .ok_or_else(|| Error::message(format!("Unknown afterFailedRun: {value}")))
    }
}
impl From<AfterFailedRun> for JsonString {
    fn from(value: AfterFailedRun) -> Self {
        value.as_str().into()
    }
}

macro_rules! json_object {
    ($($ty:ty),+) => {$(
        impl TryFrom<&$ty> for JsonObject {
            type Error = Error;
            fn try_from(value: &$ty) -> Result<Self> {
                JsonObject::try_from_value(wire::encode(value)).map_err(Error::message)
            }
        }
        impl TryFrom<&JsonObject> for $ty {
            type Error = Error;
            fn try_from(value: &JsonObject) -> Result<Self> {
                wire::decode(value.value()).map_err(Error::message)
            }
        }
    )+};
}
json_object!(
    ConversationStreamOptions,
    ConversationRetryPolicy,
    ConversationRetryPolicyPatch,
    CompactionSettings
);

impl HarnessSettings {
    pub fn with_stream(mut self, stream: &ConversationStreamOptions) -> Result<Self> {
        self.stream = Some(stream.try_into()?);
        Ok(self)
    }
    pub fn with_retry(mut self, retry: &ConversationRetryPolicyPatch) -> Result<Self> {
        self.retry = Some(retry.try_into()?);
        Ok(self)
    }
    pub fn with_compaction(mut self, compaction: &CompactionSettings) -> Result<Self> {
        self.compaction = Some(compaction.try_into()?);
        Ok(self)
    }
    pub fn with_after_failed_run(mut self, after: AfterFailedRun) -> Self {
        self.after_failed_run = Some(after.into());
        self
    }
}
impl super::settings::Settings {
    /// The resolved stream options.
    pub fn stream_options(&self) -> Result<ConversationStreamOptions> {
        (&self.stream).try_into()
    }
    /// The resolved retry policy, defaults merged.
    pub fn retry_policy(&self) -> Result<ConversationRetryPolicy> {
        (&self.retry).try_into()
    }
    /// The resolved harness-wide compaction settings, defaults merged.
    pub fn compaction_settings(&self) -> Result<CompactionSettings> {
        (&self.compaction).try_into()
    }
    pub fn after_failed_run_kind(&self) -> Result<AfterFailedRun> {
        (&self.after_failed_run).try_into()
    }
}
impl ConversationOptions {
    pub fn with_agent_change(mut self, change: &AgentChange) -> Result<Self> {
        self.agent = Some(change.try_into()?);
        Ok(self)
    }
}
impl ConversationCreateOptions {
    pub fn with_agent_change(mut self, change: &AgentChange) -> Result<Self> {
        self.agent = Some(change.try_into()?);
        Ok(self)
    }
}
impl Conversation {
    /// `configure` with a typed change.
    pub fn configure_change(
        &self,
        change: &AgentChange,
        context: &Context,
    ) -> crate::storage::StorageFuture<()> {
        match JsonObject::try_from(change) {
            Ok(change) => self.configure(change, context),
            Err(error) => Box::pin(async { Err(error) }),
        }
    }
}
