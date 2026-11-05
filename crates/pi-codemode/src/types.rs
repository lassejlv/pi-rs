//! Public types shared by every sandbox.

use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::abort::AbortSignal;

/// A JSON Schema document (an object or a boolean). Only used to render declarations; values are
/// not validated against it.
pub type CodemodeJsonSchema = Value;

/// Passed to a tool's `execute`.
#[derive(Clone, Debug)]
pub struct CodemodeToolContext {
    /// Cancelled when the script finishes (including unawaited calls), the execution times out,
    /// the caller aborts, or the sandbox is closed.
    pub signal: CancellationToken,
}

/// What a tool resolves to: `Ok(None)` is `undefined`. An `Err` surfaces in the script as an
/// `Error` with that message.
pub type CodemodeToolResult = Result<Option<Value>, String>;

/// A tool's implementation. `args` is whatever the script passed after a JSON round trip
/// (`None` for `undefined`); for spread globals the argument list as an array.
pub type CodemodeToolExecute = Arc<dyn Fn(Option<Value>, CodemodeToolContext) -> BoxFuture<'static, CodemodeToolResult> + Send + Sync>;

/// The model-facing description of a tool or global: everything but `execute`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeToolDeclaration {
    /// See [`CodemodeTool::name`].
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<CodemodeJsonSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<CodemodeJsonSchema>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub spread: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// A function scripts can call: a tool (`tools.<name>(args)`) or a global (`<name>(args)`).
#[derive(Clone)]
pub struct CodemodeTool {
    /// The script calls tools as `tools.<id>(args)`, where `<id>` is the name with characters that
    /// are not valid in identifiers replaced by `_` (see [`crate::to_codemode_identifier`]), and also
    /// as `tools["<name>"](args)`. Globals are called as `<name>(args)` and must be identifiers, or
    /// `<namespace>.<member>`, which groups them into a frozen namespace object.
    pub name: String,
    /// Shown as a doc comment in [`crate::render_declarations`], and listed in `ALL_TOOLS` for tools.
    pub description: Option<String>,
    /// Schema of the single argument. Rendered as the parameter type; `unknown` when omitted.
    pub input_schema: Option<CodemodeJsonSchema>,
    /// Schema of the resolved value. Rendered as the promise type; `unknown` when omitted.
    pub output_schema: Option<CodemodeJsonSchema>,
    /// Globals only: `execute` receives all call arguments as an array instead of the first one.
    pub spread: bool,
    /// Globals only: TypeScript parameter list and return type for [`crate::render_declarations`],
    /// for example `(type: string, id?: string): Promise<Model[]>`. Replaces the rendering from the
    /// schemas.
    pub signature: Option<String>,
    /// Runs a call. The return value must be JSON; an `Err` surfaces in the script as an `Error`
    /// with the same message.
    pub execute: CodemodeToolExecute,
}

impl std::fmt::Debug for CodemodeTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodemodeTool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("spread", &self.spread)
            .finish_non_exhaustive()
    }
}

impl CodemodeTool {
    /// A tool named `name` that runs `execute`.
    pub fn new<F, Fut>(name: impl Into<String>, execute: F) -> Self
    where
        F: Fn(Option<Value>, CodemodeToolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = CodemodeToolResult> + Send + 'static,
    {
        CodemodeTool {
            name: name.into(),
            description: None,
            input_schema: None,
            output_schema: None,
            spread: false,
            signature: None,
            execute: Arc::new(move |args, context| Box::pin(execute(args, context))),
        }
    }

    /// A tool whose `execute` answers synchronously.
    pub fn new_sync<F>(name: impl Into<String>, execute: F) -> Self
    where
        F: Fn(Option<Value>, CodemodeToolContext) -> CodemodeToolResult + Send + Sync + 'static,
    {
        let execute = Arc::new(execute);
        Self::new(name, move |args, context| {
            let result = execute(args, context);
            async move { result }
        })
    }

    /// Attach a declaration's fields (`name` included) to `execute`.
    pub fn from_declaration(declaration: CodemodeToolDeclaration, execute: CodemodeToolExecute) -> Self {
        CodemodeTool {
            name: declaration.name,
            description: declaration.description,
            input_schema: declaration.input_schema,
            output_schema: declaration.output_schema,
            spread: declaration.spread,
            signature: declaration.signature,
            execute,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_input_schema(mut self, schema: CodemodeJsonSchema) -> Self {
        self.input_schema = Some(schema);
        self
    }

    pub fn with_output_schema(mut self, schema: CodemodeJsonSchema) -> Self {
        self.output_schema = Some(schema);
        self
    }

    pub fn with_spread(mut self, spread: bool) -> Self {
        self.spread = spread;
        self
    }

    pub fn with_signature(mut self, signature: impl Into<String>) -> Self {
        self.signature = Some(signature.into());
        self
    }

    /// The tool without `execute`.
    pub fn declaration(&self) -> CodemodeToolDeclaration {
        CodemodeToolDeclaration {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            output_schema: self.output_schema.clone(),
            spread: self.spread,
            signature: self.signature.clone(),
        }
    }
}

/// Read access to the declaration fields of [`CodemodeTool`] and [`CodemodeToolDeclaration`],
/// used by the declaration renderers.
pub trait ToolDeclaration {
    fn name(&self) -> &str;
    fn description(&self) -> Option<&str>;
    fn input_schema(&self) -> Option<&CodemodeJsonSchema>;
    fn output_schema(&self) -> Option<&CodemodeJsonSchema>;
    fn signature(&self) -> Option<&str>;
}

impl ToolDeclaration for CodemodeTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
    fn input_schema(&self) -> Option<&CodemodeJsonSchema> {
        self.input_schema.as_ref()
    }
    fn output_schema(&self) -> Option<&CodemodeJsonSchema> {
        self.output_schema.as_ref()
    }
    fn signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }
}

impl ToolDeclaration for CodemodeToolDeclaration {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
    fn input_schema(&self) -> Option<&CodemodeJsonSchema> {
        self.input_schema.as_ref()
    }
    fn output_schema(&self) -> Option<&CodemodeJsonSchema> {
        self.output_schema.as_ref()
    }
    fn signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }
}

impl<T: ToolDeclaration + ?Sized> ToolDeclaration for &T {
    fn name(&self) -> &str {
        (**self).name()
    }
    fn description(&self) -> Option<&str> {
        (**self).description()
    }
    fn input_schema(&self) -> Option<&CodemodeJsonSchema> {
        (**self).input_schema()
    }
    fn output_schema(&self) -> Option<&CodemodeJsonSchema> {
        (**self).output_schema()
    }
    fn signature(&self) -> Option<&str> {
        (**self).signature()
    }
}

/// One item of the script's output, in the order the script produced it: `text()` and
/// `console.*` produce text items, with `console: Some(true)` for `console.*`, and `image()` image
/// items. `data` is base64.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CodemodeOutputItem {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        console: Option<bool>,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

impl CodemodeOutputItem {
    /// A text item (`text()`).
    pub fn text(text: impl Into<String>) -> Self {
        CodemodeOutputItem::Text { text: text.into(), console: None }
    }

    /// Characters of text or base64 data (UTF-16 code units, like the output limit counts them).
    pub fn chars(&self) -> usize {
        match self {
            CodemodeOutputItem::Text { text, .. } => crate::js::utf16_len(text),
            CodemodeOutputItem::Image { data, .. } => data.len(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeCallStatus {
    Ok,
    Error,
    Cancelled,
}

/// A tool call the script made.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeCall {
    pub name: String,
    pub status: CodemodeCallStatus,
    pub duration_ms: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeErrorKind {
    /// The script threw or failed to parse. `name` and `stack` come from the script's error.
    Script,
    /// The overall deadline expired, or the script used up its loop iteration limit. The execution
    /// ended and the VM was asked to stop; its thread may still be computing.
    Timeout,
    /// The caller's signal fired or the sandbox was closed. The execution ended and the VM was asked
    /// to stop.
    Aborted,
    /// The VM or its transport failed outside the script's control (for example a VM that could not
    /// start, or a remote sandbox that failed).
    Sandbox,
}

impl CodemodeErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CodemodeErrorKind::Script => "script",
            CodemodeErrorKind::Timeout => "timeout",
            CodemodeErrorKind::Aborted => "aborted",
            CodemodeErrorKind::Sandbox => "sandbox",
        }
    }
}

/// Why an execution failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeError {
    pub kind: CodemodeErrorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

impl CodemodeError {
    pub fn new(kind: CodemodeErrorKind, message: impl Into<String>) -> Self {
        CodemodeError { kind, name: None, message: message.into(), stack: None }
    }
}

impl std::fmt::Display for CodemodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Some(name) if !name.is_empty() => write!(f, "{name}: {}", self.message),
            _ => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for CodemodeError {}

/// Keys the script changed with `store()`. Only successful executions report writes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CodemodeStoreWrites {
    pub set: Map<String, Value>,
    /// Keys stored as `undefined`.
    pub delete: Vec<String>,
}

impl CodemodeStoreWrites {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.delete.is_empty()
    }

    /// Apply the writes to a store map.
    pub fn apply_to(&self, store: &mut Map<String, Value>) {
        for key in &self.delete {
            store.remove(key);
        }
        for (key, value) in &self.set {
            store.insert(key.clone(), value.clone());
        }
    }
}

/// In a [`crate::CodemodeSandbox::start`] run: the result the VM was processing when the script
/// finished, 0 for its first run, and the calls issued by then.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodeDuring {
    pub seq: u64,
    pub issued: u64,
}

/// A successful execution's value and store writes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodemodeSuccess {
    /// The returned value after a JSON round trip; `None` for `undefined` (also after `exit()`).
    pub value: Option<Value>,
    pub store_writes: CodemodeStoreWrites,
}

/// The result of an execution. `output` is kept for failed executions too, up to the failure.
#[derive(Clone, Debug, PartialEq)]
pub struct CodemodeResult {
    pub outcome: Result<CodemodeSuccess, CodemodeError>,
    pub output: Vec<CodemodeOutputItem>,
    pub calls: Vec<CodemodeCall>,
    /// In a [`crate::CodemodeSandbox::start`] run that the script itself ended (it returned, threw,
    /// or exited), the result being processed when it did.
    pub during: Option<CodemodeDuring>,
}

impl CodemodeResult {
    /// `result.ok`.
    pub fn ok(&self) -> bool {
        self.outcome.is_ok()
    }

    /// The returned value of a successful execution (`None` for `undefined` or a failure).
    pub fn value(&self) -> Option<&Value> {
        self.outcome.as_ref().ok().and_then(|success| success.value.as_ref())
    }

    /// The error of a failed execution.
    pub fn error(&self) -> Option<&CodemodeError> {
        self.outcome.as_ref().err()
    }

    /// The store writes of a successful execution.
    pub fn store_writes(&self) -> Option<&CodemodeStoreWrites> {
        self.outcome.as_ref().ok().map(|success| &success.store_writes)
    }
}

/// Serializes in the TS shape: `{ ok, value?, error?, output, calls, storeWrites?, during? }`.
impl Serialize for CodemodeResult {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        match &self.outcome {
            Ok(success) => {
                map.serialize_entry("ok", &true)?;
                if let Some(value) = &success.value {
                    map.serialize_entry("value", value)?;
                }
                map.serialize_entry("output", &self.output)?;
                map.serialize_entry("calls", &self.calls)?;
                map.serialize_entry("storeWrites", &success.store_writes)?;
            }
            Err(error) => {
                map.serialize_entry("ok", &false)?;
                map.serialize_entry("error", error)?;
                map.serialize_entry("output", &self.output)?;
                map.serialize_entry("calls", &self.calls)?;
            }
        }
        if let Some(during) = &self.during {
            map.serialize_entry("during", during)?;
        }
        map.end()
    }
}

/// Whether a call goes to a tool or a global.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeCallTarget {
    Tool,
    Global,
}

/// A call the script made in a [`crate::CodemodeSandbox::start`] run, answered with
/// [`CodemodeRun::deliver`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodemodeHostCall {
    /// The VM's own number for the call, `1`, `2`, ... in the order the script made its calls
    /// (tools and globals).
    pub id: u64,
    pub target: CodemodeCallTarget,
    pub name: String,
    /// What the script passed, after a JSON round trip; for spread globals the argument list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

/// A tool as a run's script sees it: `tools.<name>` and its `ALL_TOOLS` entry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeDeclaredTool {
    pub name: String,
    /// The identifier the script uses, `tools.<js_name>`. Default:
    /// `to_codemode_identifier(name)`. A host that replays a run passes the one the run started
    /// with, so a changed mapping cannot rename a tool under a resumed script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub js_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl CodemodeDeclaredTool {
    pub fn new(name: impl Into<String>) -> Self {
        CodemodeDeclaredTool { name: name.into(), ..Default::default() }
    }
}

/// A global as a run's script sees it; the same names as `globals` allow.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodeDeclaredGlobal {
    pub name: String,
    #[serde(default)]
    pub spread: bool,
}

/// A call's answer. A rejection becomes an `Error` with `message` in the script, carrying the own
/// properties of `properties` (a JSON object; `name`, `message`, and `stack` are ignored), for
/// example `{ structuredOutput }`.
#[derive(Clone, Debug, PartialEq)]
pub enum CodemodeOutcome {
    Ok { value: Option<Value> },
    Err { message: String, properties: Option<Value> },
}

impl CodemodeOutcome {
    pub fn ok(value: impl Into<Option<Value>>) -> Self {
        CodemodeOutcome::Ok { value: value.into() }
    }

    pub fn err(message: impl Into<String>) -> Self {
        CodemodeOutcome::Err { message: message.into(), properties: None }
    }
}

pub type OnStarted = Arc<dyn Fn(&str) + Send + Sync>;
/// Receives each call and a handle to answer it with.
pub type OnCall = Arc<dyn Fn(CodemodeHostCall, CodemodeRunHandle) + Send + Sync>;
pub type OnIdle = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// Options of [`crate::CodemodeSandbox::start`].
#[derive(Clone)]
pub struct CodemodeRunOptions {
    /// The VM clock's start, in ms since the epoch: `Date.now()` until the first result arrives.
    pub started_at: f64,
    /// Seeds `Math.random()`.
    pub seed: u32,
    pub tools: Vec<CodemodeDeclaredTool>,
    pub globals: Vec<CodemodeDeclaredGlobal>,
    pub store: Option<Map<String, Value>>,
    pub signal: Option<AbortSignal>,
    /// Deadline of this run in ms. Default: none (not the sandbox's default).
    pub timeout_ms: Option<f64>,
    /// The VM's fingerprint, before the script runs and before any of its calls.
    pub on_started: Option<OnStarted>,
    /// The script made a call. Answer it with [`CodemodeRunHandle::deliver`], now or later.
    pub on_call: OnCall,
    /// The VM processed result `seq` (`0`: the script's first run, before any result) and waits
    /// again, having issued `issued` calls in total.
    pub on_idle: Option<OnIdle>,
}

impl CodemodeRunOptions {
    pub fn new(started_at: f64, seed: u32, on_call: impl Fn(CodemodeHostCall, CodemodeRunHandle) + Send + Sync + 'static) -> Self {
        CodemodeRunOptions {
            started_at,
            seed,
            tools: Vec::new(),
            globals: Vec::new(),
            store: None,
            signal: None,
            timeout_ms: None,
            on_started: None,
            on_call: Arc::new(on_call),
            on_idle: None,
        }
    }
}

/// Options of [`crate::CodemodeSandbox::execute`].
#[derive(Clone, Default)]
pub struct CodemodeExecuteOptions {
    pub signal: Option<AbortSignal>,
    /// Overrides the sandbox default for this execution. Non-finite values disable the deadline.
    pub timeout_ms: Option<f64>,
    /// Values the script reads with `load(key)`. The script's own `store()` calls come back as
    /// the result's store writes; persisting them is up to the caller.
    pub store: Option<Map<String, Value>>,
    /// The host timezone the script's `Date` uses (an IANA name such as `Asia/Kolkata` or a POSIX
    /// `TZ` string), like a worker reading its process's `TZ`. `None`: the VM thread's own (`TZ`
    /// in the environment). Remote VMs keep the remote's timezone.
    pub timezone: Option<String>,
}

/// A running [`crate::CodemodeSandbox::start`] execution.
pub struct CodemodeRun {
    pub(crate) result: BoxFuture<'static, CodemodeResult>,
    pub(crate) control: Arc<dyn RunControl>,
}

pub(crate) trait RunControl: Send + Sync {
    fn finished(&self) -> bool;
    fn deliver(&self, seq: u64, now: f64, id: u64, outcome: CodemodeOutcome) -> Result<(), String>;
    fn deliver_json(&self, seq: u64, now: f64, id: u64, outcome: crate::CodemodeJsonOutcome) -> Result<(), String>;
}

impl CodemodeRun {
    /// Whether the run has its outcome: the script ended, timed out, or failed, or the run was
    /// aborted. Set when that is decided, before the result resolves; calls can no longer be
    /// answered. It does not mean the VM has exited.
    pub fn finished(&self) -> bool {
        self.control.finished()
    }

    /// Answer call `id` as result number `seq`. Results are numbered 1, 2, ... without gaps, and the
    /// VM applies them strictly in that order, whatever order they arrive in. While it processes this
    /// one, its clock reads `now` (ms since the epoch), or the previous value if `now` is earlier.
    /// Fails for a call that is not waiting for an answer, a `seq` used before, or a `now` that is
    /// not finite. Does nothing once the run is finished.
    pub fn deliver(&self, seq: u64, now: f64, id: u64, outcome: CodemodeOutcome) -> Result<(), String> {
        self.control.deliver(seq, now, id, outcome)
    }

    /// A handle that can deliver results from elsewhere (for example from `on_call`).
    pub fn handle(&self) -> CodemodeRunHandle {
        CodemodeRunHandle { control: self.control.clone() }
    }

    /// The result. Never fails for script failures.
    pub async fn result(self) -> CodemodeResult {
        self.result.await
    }
}

/// A cloneable handle on a run, for [`CodemodeRun::deliver`] and [`CodemodeRun::finished`].
#[derive(Clone)]
pub struct CodemodeRunHandle {
    control: Arc<dyn RunControl>,
}

impl CodemodeRunHandle {
    pub(crate) fn new(control: Arc<dyn RunControl>) -> Self {
        CodemodeRunHandle { control }
    }

    pub fn finished(&self) -> bool {
        self.control.finished()
    }

    pub fn deliver(&self, seq: u64, now: f64, id: u64, outcome: CodemodeOutcome) -> Result<(), String> {
        self.control.deliver(seq, now, id, outcome)
    }
}
