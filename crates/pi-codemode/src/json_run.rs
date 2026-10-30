//! Deterministic runs retaining JavaScript UTF-16 through every JSON value.
use crate::*;
use futures::future::BoxFuture;
use pi_json::strict::{self, JsonString, Map, Value};
use std::sync::Arc;

/// Script-visible tool metadata retaining every JavaScript string code unit.
#[derive(Clone, Debug, Default)]
pub struct CodemodeJsonDeclaredTool {
    pub name: JsonString,
    pub js_name: Option<JsonString>,
    pub description: Option<JsonString>,
}
impl CodemodeJsonDeclaredTool {
    pub fn new(name: impl Into<JsonString>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }
}
impl From<CodemodeDeclaredTool> for CodemodeJsonDeclaredTool {
    fn from(tool: CodemodeDeclaredTool) -> Self {
        Self {
            name: tool.name.into(),
            js_name: tool.js_name.map(Into::into),
            description: tool.description.map(Into::into),
        }
    }
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JsonCallRecord {
    pub name: JsonString,
    pub status: CodemodeCallStatus,
    pub duration_ms: f64,
}
#[derive(Clone, Debug)]
pub struct CodemodeJsonHostCall {
    pub id: u64,
    pub target: CodemodeCallTarget,
    pub name: JsonString,
    pub args: Option<Value>,
}
#[derive(Clone, Debug)]
pub enum CodemodeJsonOutcome {
    Ok {
        value: Option<Value>,
    },
    Err {
        message: JsonString,
        properties: Option<Map>,
    },
}
pub type OnJsonCall = Arc<dyn Fn(CodemodeJsonHostCall, CodemodeJsonRunHandle) + Send + Sync>;
#[derive(Clone)]
pub struct CodemodeJsonRunOptions {
    pub started_at: f64,
    pub seed: u32,
    pub tools: Vec<CodemodeJsonDeclaredTool>,
    pub globals: Vec<CodemodeDeclaredGlobal>,
    pub store: Option<Map>,
    pub signal: Option<AbortSignal>,
    pub timeout_ms: Option<f64>,
    pub on_started: Option<OnStarted>,
    pub on_call: OnJsonCall,
    pub on_idle: Option<OnIdle>,
}
impl CodemodeJsonRunOptions {
    pub fn new(
        started_at: f64,
        seed: u32,
        on_call: impl Fn(CodemodeJsonHostCall, CodemodeJsonRunHandle) + Send + Sync + 'static,
    ) -> Self {
        Self {
            started_at,
            seed,
            on_call: Arc::new(on_call),
            tools: vec![],
            globals: vec![],
            store: None,
            signal: None,
            timeout_ms: None,
            on_started: None,
            on_idle: None,
        }
    }
}
pub struct CodemodeJsonRun {
    pub(crate) result: BoxFuture<'static, Value>,
    pub(crate) handle: CodemodeJsonRunHandle,
}
impl CodemodeJsonRun {
    pub fn finished(&self) -> bool {
        self.handle.finished()
    }
    pub fn handle(&self) -> CodemodeJsonRunHandle {
        self.handle.clone()
    }
    pub fn deliver(
        &self,
        seq: u64,
        now: f64,
        id: u64,
        outcome: CodemodeJsonOutcome,
    ) -> Result<(), String> {
        self.handle.deliver(seq, now, id, outcome)
    }
    pub async fn result(self) -> Value {
        self.result.await
    }
}
#[derive(Clone)]
pub struct CodemodeJsonRunHandle {
    control: Arc<dyn crate::types::RunControl>,
}
impl CodemodeJsonRunHandle {
    pub(crate) fn new(control: Arc<dyn crate::types::RunControl>) -> Self {
        Self { control }
    }
    pub fn finished(&self) -> bool {
        self.control.finished()
    }
    pub fn deliver(
        &self,
        seq: u64,
        now: f64,
        id: u64,
        outcome: CodemodeJsonOutcome,
    ) -> Result<(), String> {
        self.control.deliver_json(seq, now, id, outcome)
    }
}
pub(crate) fn put(value: &mut Value, key: &str, field: impl Into<Value>) {
    value
        .as_object_mut()
        .expect("result object")
        .insert(key.into(), field.into());
}
pub(crate) fn encoded<T: serde::Serialize + ?Sized>(value: &T) -> Value {
    strict::to_value(value).expect("code-mode records are JSON serializable")
}
pub(crate) fn decode_result(value: Value) -> Result<CodemodeResult, String> {
    fn field<T: serde::de::DeserializeOwned>(value: &Value, name: &str) -> Result<T, String> {
        serde_json::from_str(&value[name].to_string()).map_err(|e| e.to_string())
    }
    let outcome = if value["ok"].as_bool() == Some(true) {
        let returned = value
            .as_object()
            .and_then(|v| v.get(&JsonString::from("value")))
            .map(|v| serde_json::from_str(&v.to_string()).map_err(|e| e.to_string()))
            .transpose()?;
        Ok(CodemodeSuccess {
            value: returned,
            store_writes: field(&value, "storeWrites")?,
        })
    } else {
        Err(field(&value, "error")?)
    };
    let during = value
        .as_object()
        .and_then(|v| v.get(&JsonString::from("during")))
        .map(|v| serde_json::from_str(&v.to_string()).map_err(|e| e.to_string()))
        .transpose()?;
    Ok(CodemodeResult {
        outcome,
        output: field(&value, "output")?,
        calls: field(&value, "calls")?,
        during,
    })
}
pub(crate) fn done_json(done: &DoneMessage) -> Result<Value, String> {
    let parse = |text: &str, what: &str| {
        strict::parse(text).map_err(|_| format!("{what} is not valid JSON"))
    };
    let mut result = Value::Object(Map::new().into());
    put(&mut result, "ok", Value::Bool(done.ok));
    if !done.ok {
        let error = parse(done.error.as_deref().unwrap_or("undefined"), "script error")?;
        if error.is_array() {
            return Err("script error is malformed".into());
        }
        let object = error.as_object().ok_or("script error is not an object")?;
        if !object
            .get(&JsonString::from("message"))
            .is_some_and(Value::is_string)
            || ["name", "stack"].iter().any(|k| {
                object
                    .get(&JsonString::from(*k))
                    .is_some_and(|v| !v.is_string())
            })
        {
            return Err("script error is malformed".into());
        }
        let mut fields = Map::new();
        fields.insert("kind".into(), "script".into());
        for name in ["name", "message", "stack"] {
            if let Some(value) = object.get(&JsonString::from(name)) {
                fields.insert(name.into(), value.clone());
            }
        }
        put(&mut result, "error", Value::Object(fields.into()));
        return Ok(result);
    }
    if let Some(value) = &done.value {
        put(&mut result, "value", parse(value, "return value")?);
    }
    let entries = parse(
        done.writes.as_deref().unwrap_or("undefined"),
        "store writes",
    )?;
    let entries = entries.as_array().ok_or("store writes are not an array")?;
    let mut set = Map::new();
    let mut delete = vec![];
    for entry in entries {
        let entry = entry
            .as_array()
            .ok_or("store writes contain a malformed entry")?;
        let key = entry
            .first()
            .and_then(Value::as_string)
            .ok_or("store writes contain a malformed entry")?
            .clone();
        match entry.len() {
            1 => delete.push(Value::String(key)),
            2 => {
                let text = entry[1]
                    .to_utf8()
                    .ok_or("store writes contain a malformed entry")?;
                let value = parse(
                    &text,
                    &format!("store value for {}", Value::String(key.clone())),
                )?;
                set.insert(key, value);
            }
            _ => return Err("store writes contain a malformed entry".into()),
        }
    }
    put(
        &mut result,
        "storeWrites",
        Value::Object(
            [
                (JsonString::from("set"), Value::Object(set.into())),
                ("delete".into(), Value::Array(delete.into())),
            ]
            .into_iter()
            .collect::<Map>()
            .into(),
        ),
    );
    Ok(result)
}
