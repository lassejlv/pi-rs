//! Point-in-time diagnostics derived on the Session line without executing tasks.
use super::scheduler::BlockedReason;
use crate::*;
use serde::{Serialize, Serializer};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheduling {
    Paused,
    Running,
    Closing,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TaskInspectionState {
    Running,
    Ready {
        migrates: bool,
    },
    Waiting {
        on: Vec<TaskId>,
    },
    Sleeping {
        until: f64,
    },
    Completing,
    Blocked {
        reason: BlockedReason,
        #[serde(
            skip_serializing_if = "Option::is_none",
            serialize_with = "serialize_error"
        )]
        error: Option<Error>,
    },
}
fn serialize_error<S: Serializer>(
    error: &Option<Error>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    error
        .as_ref()
        .map(|error| serde_json::json!({"name":error.name(),"message":error.to_string()}))
        .serialize(serializer)
}
#[derive(Clone, Debug, Serialize)]
pub struct TaskInspection {
    pub record: std::sync::Arc<TaskRecord>,
    pub state: TaskInspectionState,
}
#[derive(Clone, Debug, Serialize)]
pub struct HarnessInspection {
    pub scheduling: Scheduling,
    pub tasks: Vec<TaskInspection>,
    pub submissions: Vec<SubmissionRecord>,
}
