//! Portable command execution, output delivery and command-local cancellation.
use crate::{Context, JsonObject, JsonString, JsonValue, wire};
pub use pi_chord::output::OutputSkip as ShellOutputSkip;
use serde::{Deserialize, Serialize};
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionErrorCode {
    Aborted,
    Timeout,
    ShellUnavailable,
    SpawnError,
    CallbackError,
    Unknown,
}
impl ExecutionErrorCode {
    pub const ALL: [Self; 6] = [
        Self::Aborted,
        Self::Timeout,
        Self::ShellUnavailable,
        Self::SpawnError,
        Self::CallbackError,
        Self::Unknown,
    ];
    /// The reference's code string (`ExecutionErrorCode` in `env/index.ts`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::Timeout => "timeout",
            Self::ShellUnavailable => "shell_unavailable",
            Self::SpawnError => "spawn_error",
            Self::CallbackError => "callback_error",
            Self::Unknown => "unknown",
        }
    }
    /// The code for a reference code string; anything else is `Unknown`.
    pub fn from_name(name: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|code| code.as_str() == name)
            .unwrap_or(Self::Unknown)
    }
}
#[derive(Debug)]
struct Failure {
    code: ExecutionErrorCode,
    message: String,
    spill_path: Option<PathBuf>,
    cause: Option<Arc<dyn std::error::Error + Send + Sync>>,
}
#[derive(Clone, Debug)]
pub struct ExecutionError(Arc<Failure>);
impl ExecutionError {
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> Self {
        Self(Arc::new(Failure {
            code,
            message: message.into(),
            spill_path: None,
            cause: None,
        }))
    }
    pub fn with_spill_path(self, spill_path: PathBuf) -> Self {
        Self(Arc::new(Failure {
            code: self.0.code,
            message: self.0.message.clone(),
            spill_path: Some(spill_path),
            cause: self.0.cause.clone(),
        }))
    }
    pub fn with_cause(self, cause: Arc<dyn std::error::Error + Send + Sync>) -> Self {
        Self(Arc::new(Failure {
            code: self.0.code,
            message: self.0.message.clone(),
            spill_path: self.0.spill_path.clone(),
            cause: Some(cause),
        }))
    }
    pub fn code(&self) -> ExecutionErrorCode {
        self.0.code
    }
    pub fn spill_path(&self) -> Option<&std::path::Path> {
        self.0.spill_path.as_deref()
    }
    pub fn same_error(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl std::fmt::Display for ExecutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0.message)
    }
}
impl std::error::Error for ExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.cause.as_deref().map(|cause| cause as _)
    }
}
pub type ExecutionResult<T> = std::result::Result<T, ExecutionError>;
pub type ExecutionFuture<T> = Pin<Box<dyn Future<Output = ExecutionResult<T>> + Send + 'static>>;

#[derive(Clone, Debug)]
pub enum ShellCommand {
    Script(JsonString),
    Argv(Vec<JsonString>),
}
impl ShellCommand {
    fn to_json(&self) -> JsonValue {
        match self {
            Self::Script(script) => wire::encode(script),
            Self::Argv(arguments) => wire::encode(arguments),
        }
    }
    fn from_json(value: &JsonValue) -> std::result::Result<Self, String> {
        if value.is_string() {
            wire::decode(value).map(Self::Script)
        } else {
            wire::decode(value).map(Self::Argv)
        }
    }
}
wire::raw_serde!(ShellCommand);
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellSpillOptions {
    pub after_bytes: f64,
    pub after_lines: f64,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutputWindow {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub min_interval_ms: f64,
    pub bytes_per_second: f64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellStream {
    Stdout,
    Stderr,
}
#[derive(Clone, Copy, Debug)]
pub struct ShellOutputInfo {
    pub stream: ShellStream,
    pub skipped: Option<ShellOutputSkip>,
}
pub type ShellOutput =
    Arc<dyn Fn(JsonString, Context, ShellOutputInfo) -> ExecutionResult<()> + Send + Sync>;
#[derive(Clone, Default)]
pub struct ShellExecOptions {
    pub cwd: Option<PathBuf>,
    pub env: Option<JsonObject>,
    pub inherit_env: Option<bool>,
    pub timeout: Option<f64>,
    pub on_output: Option<ShellOutput>,
    pub spill: Option<ShellSpillOptions>,
    pub window: Option<ShellOutputWindow>,
}
#[derive(Clone, Debug)]
pub struct ShellExecResult {
    pub exit_code: i32,
    pub spill_path: Option<PathBuf>,
}
pub trait Shell: Send + Sync {
    fn exec(
        &self,
        command: ShellCommand,
        options: ShellExecOptions,
        context: &Context,
    ) -> ExecutionFuture<ShellExecResult>;
    /// Environment-owner shutdown; request cancellation affects only its command.
    fn cleanup(&self, context: &Context) -> ExecutionFuture<()>;
}
pub trait ExecutionEnv: super::FileSystem + Shell {}
impl<T: super::FileSystem + Shell> ExecutionEnv for T {}
