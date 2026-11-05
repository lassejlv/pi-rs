//! Lossless persisted usage buckets; counters use JavaScript number addition.
use crate::{wire, *};
use indexmap::IndexMap;

#[derive(Clone, Debug, Default)]
pub struct UsageState {
    pub models: IndexMap<JsonString, JsonObject>,
    pub tools: IndexMap<JsonString, JsonObject>,
}
impl UsageState {
    fn to_json(&self) -> JsonValue {
        let bucket = |values: &IndexMap<JsonString, JsonObject>| {
            JsonValue::Object(
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone().into_value()))
                    .collect::<pi_chord::json::Map>()
                    .into(),
            )
        };
        let mut value = wire::object();
        wire::put(&mut value, "models", &bucket(&self.models));
        wire::put(&mut value, "tools", &bucket(&self.tools));
        value
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        let bucket = |name: &str| -> std::result::Result<IndexMap<JsonString, JsonObject>, String> {
            let values: JsonObject = wire::field(value, name)?;
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), JsonObject::try_from_value(value.clone())?)))
                .collect()
        };
        Ok(Self {
            models: bucket("models")?,
            tools: bucket("tools")?,
        })
    }
}
wire::raw_serde!(UsageState);
impl DocumentState for UsageState {}
pub static USAGE_DOC: std::sync::LazyLock<LatestConversationDocToken<UsageState>> =
    std::sync::LazyLock::new(|| {
        define_doc(DocDefinition::new(
            "pi.usage",
            1,
            LatestConversationScope {
                fork: LatestFork::Initial,
            },
            || Ok(UsageState::default()),
            |_, _, _| true,
        ))
        .expect("valid built-in usage definition")
    });
pub fn usage_doc() -> LatestConversationDocToken<UsageState> {
    USAGE_DOC.clone()
}
#[derive(Clone, Copy, Debug)]
pub enum UsageBucket {
    Models,
    Tools,
}
impl UsageState {
    fn bucket_mut(&mut self, bucket: UsageBucket) -> &mut IndexMap<JsonString, JsonObject> {
        match bucket {
            UsageBucket::Models => &mut self.models,
            UsageBucket::Tools => &mut self.tools,
        }
    }
}
pub async fn record_usage(
    tx: &Transaction,
    conversation: ConversationId,
    bucket: UsageBucket,
    key: impl Into<JsonString>,
    usage: &JsonObject,
) -> Result<()> {
    let draft = tx.doc(&usage_doc(), conversation).await?;
    let key = key.into();
    draft.update(|state| {
        let totals = state.bucket_mut(bucket);
        if let Some(total) = totals.get_mut(&key) {
            add_usage(total, usage)
        } else {
            totals.insert(key, usage.clone());
            Ok(())
        }
    })
}
pub fn add_usage(total: &mut JsonObject, usage: &JsonObject) -> Result<()> {
    fn sum(
        total: &JsonValue,
        usage: &JsonValue,
        key: &str,
        optional: bool,
    ) -> Result<Option<JsonValue>> {
        let right = wire::get(usage, key);
        if optional && right.is_none() {
            return Ok(None);
        }
        let a = wire::get(total, key)
            .map(|v| {
                v.as_f64()
                    .ok_or_else(|| Error::message(format!("Usage {key} must be a number")))
            })
            .transpose()?
            .map_or_else(
                || {
                    if optional {
                        Ok(0.0)
                    } else {
                        Err(Error::message(format!("Usage {key} must be a number")))
                    }
                },
                Ok,
            )?;
        let b = right
            .and_then(JsonValue::as_f64)
            .ok_or_else(|| Error::message(format!("Usage {key} must be a number")))?;
        Ok(Some(JsonValue::Number(
            serde_json::Number::from_f64(a + b)
                .ok_or_else(|| Error::message(format!("Usage {key} must be finite")))?,
        )))
    }
    let mut value = total.clone().into_value();
    let source = usage.clone().into_value();
    for key in ["input", "output", "cacheRead", "cacheWrite", "totalTokens"] {
        let next = sum(&value, &source, key, false)?.unwrap();
        wire::put(&mut value, key, &next);
    }
    for key in ["cacheWrite1h", "reasoning"] {
        if let Some(next) = sum(&value, &source, key, true)? {
            wire::put(&mut value, key, &next);
        }
    }
    let mut cost: JsonObject = wire::field(&value, "cost").map_err(Error::message)?;
    let cost_source: JsonValue = wire::field(&source, "cost").map_err(Error::message)?;
    let mut costs = cost.into_value();
    for key in ["input", "output", "cacheRead", "cacheWrite", "total"] {
        let next = sum(&costs, &cost_source, key, false)?.unwrap();
        wire::put(&mut costs, key, &next);
    }
    cost = JsonObject::try_from_value(costs).map_err(Error::message)?;
    wire::put(&mut value, "cost", &cost);
    *total = JsonObject::try_from_value(value).map_err(Error::message)?;
    Ok(())
}
pub fn add_usage_state(sum: &mut UsageState, state: &UsageState) -> Result<()> {
    for (total, source) in [
        (&mut sum.models, &state.models),
        (&mut sum.tools, &state.tools),
    ] {
        for (key, usage) in source {
            if let Some(prior) = total.get_mut(key) {
                add_usage(prior, usage)?;
            } else {
                total.insert(key.clone(), usage.clone());
            }
        }
    }
    Ok(())
}
