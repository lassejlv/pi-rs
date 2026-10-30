//! The receipt of a settled task: a task record known to be terminal, as upstream's `SettledTask`.

use crate::*;

/// A task record whose state is `terminal`. Only terminal records can construct it, so the outcome
/// is always present; it dereferences to the record for every other field.
#[derive(Clone, Debug)]
pub struct SettledTask<I = JsonValue, S = JsonValue, R = JsonValue>(TaskRecord<I, S, R>);
impl<I, S, R> SettledTask<I, S, R> {
    pub fn as_record(&self) -> &TaskRecord<I, S, R> {
        &self.0
    }
    pub fn into_record(self) -> TaskRecord<I, S, R> {
        self.0
    }
    /// How the task ended.
    pub fn outcome(&self) -> &TaskOutcome<R> {
        match &self.0.state {
            TaskState::Terminal { outcome } => outcome,
            _ => unreachable!("a settled task is terminal"),
        }
    }
}
impl<I, S, R> TryFrom<TaskRecord<I, S, R>> for SettledTask<I, S, R> {
    type Error = Error;
    fn try_from(record: TaskRecord<I, S, R>) -> Result<Self> {
        if matches!(record.state, TaskState::Terminal { .. }) {
            Ok(Self(record))
        } else {
            Err(Error::message(format!(
                "Task {} has not settled",
                record.id()
            )))
        }
    }
}
impl<I, S, R> std::ops::Deref for SettledTask<I, S, R> {
    type Target = TaskRecord<I, S, R>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<I, S, R> From<SettledTask<I, S, R>> for TaskRecord<I, S, R> {
    fn from(settled: SettledTask<I, S, R>) -> Self {
        settled.0
    }
}
