//! Messages between the host and a VM. Tool arguments, results, and values cross as JSON strings:
//! the VM passes them into and out of the engine as strings and never builds structured values
//! itself. Ordinary messages retain the TS protocol's JSON. Lossless JSON runs negotiate
//! UTF-16 extensions through the VM fingerprint before host effects.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::types::{CodemodeCallTarget, CodemodeDuring, CodemodeOutputItem};

/// A tool as the VM sees it: `js_name` is the identifier the script uses; `description` is listed
/// in `ALL_TOOLS`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmTool {
    pub name: pi_json::strict::JsonString,
    pub js_name: pi_json::strict::JsonString,
    pub description: pi_json::strict::JsonString,
}

/// A global as the VM sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmGlobal {
    pub name: String,
    pub spread: bool,
}

/// Deterministic mode (`CodemodeSandbox::start()`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmDeterministic {
    pub started_at: f64,
    pub seed: u32,
}

/// Everything a VM needs to run one script, independent of where it runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmStartData {
    pub code: String,
    /// Lossless source when JavaScript text contains unpaired surrogate units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_utf16: Option<Vec<u16>>,
    pub tools: Vec<VmTool>,
    pub globals: Vec<VmGlobal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit_bytes: Option<u64>,
    /// Snapshot for `load()`: key to JSON text.
    pub store: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lossless_json: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_json: Option<String>,
    /// Run deterministically: UTC, a virtual clock starting at `started_at` (ms since the epoch)
    /// that moves only with delivered results, randomness from `seed`, no `WeakRef`, results
    /// applied in `seq` order, and `started`/`idle` messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deterministic: Option<VmDeterministic>,
    /// The timezone of a non-deterministic VM's `Date` (IANA name or POSIX `TZ` string); `None`:
    /// the VM thread's own. Never sent to a remote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// `done`: `ok` with the JSON `value` (absent for `undefined`) and `writes`, a JSON array of
/// `[key, json]` for `store()` and `[key]` for deletions; or not `ok` with `error`, the
/// JSON-encoded `{ name?, message, stack? }` of the script's error. Deterministic mode: `during` is
/// the result the VM was processing when the script finished and the calls issued by then.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DoneMessage {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub during: Option<CodemodeDuring>,
}

impl DoneMessage {
    pub fn success(value: Option<String>, writes: String) -> Self {
        DoneMessage { ok: true, value, writes: Some(writes), error: None, during: None }
    }

    pub fn failure(error: String) -> Self {
        DoneMessage { ok: false, value: None, writes: None, error: Some(error), during: None }
    }
}

/// What a VM reports to its host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkerToHostMessage {
    Call {
        id: u64,
        target: CodemodeCallTarget,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
    },
    /// Lossless call names for deterministic JSON runs.
    CallUtf16 {
        id: u64,
        target: CodemodeCallTarget,
        name: Vec<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
    },
    Output {
        item: CodemodeOutputItem,
    },
    /// UTF-16 output for a lossless JSON run. Ordinary runs retain the original protocol.
    OutputUtf16 {
        kind: String,
        data: Vec<u16>,
        #[serde(rename = "mimeType")]
        mime_type: Vec<u16>,
    },
    Done(DoneMessage),
    /// The VM failed outside the script's control.
    Crash {
        message: String,
    },
    /// Deterministic mode, before the script runs: what the VM is, so a replay can refuse a
    /// different one.
    Started {
        fingerprint: String,
    },
    /// Deterministic mode, after the script waits again: having processed result `seq` (0: the
    /// script's first run), it has issued `issued` calls in total.
    Idle {
        seq: u64,
        issued: u64,
    },
}

impl WorkerToHostMessage {
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, WorkerToHostMessage::Done(_) | WorkerToHostMessage::Crash { .. })
    }
}

/// A call's result. `payload` is the JSON result when `ok` (absent for `undefined`), otherwise the
/// error message. Deterministic mode: `seq` numbers the results from 1 in the order the VM applies
/// them, the VM's clock reads `now` while it processes this one, and `properties` (`ok: false`)
/// is a JSON object whose properties the script's `Error` gets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum HostToWorkerMessage {
    ResultUtf16 {
        id: u64,
        ok: bool,
        payload: Option<Vec<u16>>,
        seq: u64,
        now: f64,
        properties: Option<String>,
    },
    Result {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        now: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        properties: Option<String>,
    },
}

impl HostToWorkerMessage {
    pub fn id(&self) -> u64 {
        match self {
            HostToWorkerMessage::Result { id, .. } | HostToWorkerMessage::ResultUtf16 { id, .. } => *id,
        }
    }
}

/// `isWorkerToHostMessage`: decode a message received as JSON.
pub fn parse_worker_to_host_message(value: serde_json::Value) -> Option<WorkerToHostMessage> {
    serde_json::from_value(value).ok()
}

/// `isHostToWorkerMessage`: decode a message received as JSON.
pub fn parse_host_to_worker_message(value: serde_json::Value) -> Option<HostToWorkerMessage> {
    serde_json::from_value(value).ok()
}
